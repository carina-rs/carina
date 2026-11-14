//! Validation utilities for resources and modules

use crate::resource::{DiagnosticPath, DiagnosticScope, InsideSecret, SecretDiagnosticError};
pub use crate::schema::value_validation::{
    SymbolicDiagnosticCoverage, SymbolicFinding, SymbolicFindingOrigin,
};
pub mod admission;
pub mod deferred_populate;
pub mod deferred_types;
pub mod depends_on;
mod directive_keys;
pub use directive_keys::{UnknownDirectiveKey, unknown_directive_keys};
mod parameter_findings;
pub mod routing;
pub mod typecheck;
pub use parameter_findings::{ParameterValueError, ParameterValueFinding};
pub mod wait;

use crate::resource::ValueKind;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use indexmap::IndexMap;

use crate::binding_index::{
    BindingIndex, RefType, RefTypeAssignabilityError, RefTypeError, ResolvedRefType,
};
use crate::deps::collect_dependencies;
use crate::parser::{
    ForBinding, LoadedProviderContext, ModuleCall, ModuleCallId, ParsedValue as Value,
    ProviderContext, ResourceRef, ResourceTypePath, SourceFileId, SourceSpan, TypeExpr,
};
use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, CompositionCallPath, EnumValueResolver, ModuleSource,
    ParserPlaceholder, PlaceholderKind, RawEnumIdentifier, ReferencePath, ReferencePathRef,
    RelativeBindingName, Resource as GenericResource, Value as GenericValue,
};
use crate::schema::{
    BindingKind, BindingShadowsEnumMemberError, CompiledAttributeType as AttributeType,
    FieldAncestry, MismatchTypeNames, SchemaRegistry, Shape, TypeError, TypeIdentity,
};
use crate::static_type::{
    ConsumerExportStaticType, ConsumerInferredUncheckedReason, ConsumerResolvedType,
    ConsumerStaticType, ExportStaticType, StaticType, StaticTypeResolutionError,
    UnnameableInConsumer,
};

/// Lift a module-boundary [`TypeExpr`] into the schema type system.
///
/// The returned type is definition-free and can therefore be paired with
/// [`crate::schema::TypeInSchema::schemaless`]. Callers must still choose the
/// direction of assignment explicitly: a declared input/output type used as a
/// sink belongs on the right-hand side of `source.is_assignable_to(sink)`,
/// while a declared upstream type used as a source belongs on the left.
///
/// String literals retain a closed-set marker while remaining assignable to a
/// plain string. This supplies #2648's scalar shape without letting a lifted
/// resource handle satisfy a closed set by accident. Resolved resource
/// annotations (`Ref`) lift to a distinct handle kind. A
/// `DottedUnresolved` declaration is still ambiguous between a resource type
/// and a provider-owned value type, so it remains unchecked until declaration
/// resolution classifies it. The handle's path is retained for BindingIndex's
/// single declared-type assignability relation; it is never compatible with a
/// value type.
/// Built-ins retain their constructor declaration. Provider identities carry
/// the scalar kind established when the schema type was constructed.
/// Unloaded schema types retain nominal evidence for directional identity
/// checks; missing scalar or edge evidence remains `Unchecked(ProvidersNotLoaded)`.
/// Each identified leaf retains its owners. Use [`LiftedType::assignment_sink`]
/// to bind the catalog when this declaration is a sink.
pub struct LiftedType {
    attr: AttributeType,
}

impl LiftedType {
    pub fn assignment_sink<'a>(
        &'a self,
        catalog: &'a crate::parser::ProviderCustomTypes,
    ) -> crate::schema::AssignmentSink<'a, crate::schema::Compiled> {
        crate::schema::AssignmentSink::annotation(self, catalog)
    }

    pub fn into_attribute_type(self) -> AttributeType {
        self.attr
    }
}

impl std::ops::Deref for LiftedType {
    type Target = AttributeType;
    fn deref(&self) -> &Self::Target {
        &self.attr
    }
}

/// Lift a resolved declaration while preserving every identified leaf's owners.
pub fn lift_type_expr(type_expr: &TypeExpr) -> Option<LiftedType> {
    lift_attribute_type(type_expr).map(|attr| LiftedType { attr })
}

fn lift_attribute_type(type_expr: &TypeExpr) -> Option<AttributeType> {
    use crate::schema::CompiledStructField as StructField;

    match type_expr {
        TypeExpr::String => Some(AttributeType::string()),
        TypeExpr::StringLiteral(value) => {
            Some(AttributeType::string_literal_set(vec![value.clone()]))
        }
        TypeExpr::Bool => Some(AttributeType::bool()),
        TypeExpr::Int => Some(AttributeType::int()),
        TypeExpr::Float => Some(AttributeType::float()),
        TypeExpr::Duration => Some(AttributeType::duration()),
        TypeExpr::Simple(name) => Some(name.declaration().clone()),
        TypeExpr::UnloadedSchemaType(unloaded) => Some(AttributeType {
            kind: crate::schema::AttrTypeKind::UnloadedSchemaType(unloaded.clone()),
        }),
        TypeExpr::SchemaType { owned } => Some(owned.materialize()),
        TypeExpr::List(inner) => lift_attribute_type(inner).map(AttributeType::list),
        TypeExpr::Map(inner) => lift_attribute_type(inner).map(AttributeType::map),
        TypeExpr::Struct { fields } => fields
            .iter()
            .map(|(name, field_type)| {
                lift_attribute_type(field_type)
                    .map(|field_type| StructField::optional(name.clone(), field_type).build())
            })
            .collect::<Option<Vec<_>>>()
            .map(|fields| AttributeType::struct_("anonymous", fields)),
        TypeExpr::Union(members)
            if !members.is_empty()
                && members
                    .iter()
                    .all(|member| matches!(member, TypeExpr::StringLiteral(_))) =>
        {
            Some(AttributeType::string_literal_set(
                members
                    .iter()
                    .filter_map(|member| match member {
                        TypeExpr::StringLiteral(value) => Some(value.clone()),
                        TypeExpr::String
                        | TypeExpr::Bool
                        | TypeExpr::Int
                        | TypeExpr::Float
                        | TypeExpr::Duration
                        | TypeExpr::Simple(_)
                        | TypeExpr::UnloadedSchemaType(_)
                        | TypeExpr::List(_)
                        | TypeExpr::Map(_)
                        | TypeExpr::Ref(_)
                        | TypeExpr::DottedUnresolved(_)
                        | TypeExpr::SchemaType { owned: _ }
                        | TypeExpr::Struct { fields: _ }
                        | TypeExpr::Union(_) => None,
                    })
                    .collect(),
            ))
        }
        TypeExpr::Union(members) => members
            .iter()
            .map(lift_attribute_type)
            .collect::<Option<Vec<_>>>()
            .filter(|members| !members.is_empty())
            .map(AttributeType::union),
        TypeExpr::Ref(path) => Some(AttributeType::resource_handle(
            path.provider.clone(),
            path.resource_type.clone(),
        )),
        TypeExpr::DottedUnresolved(_) => None,
    }
}

// Interpolation renders scalar operands to text; collections and handles
// cannot satisfy this boundary. Keep the operand contract separate from the
// String type of the completed interpolation.
static INTERPOLATION_REF_SINK: std::sync::LazyLock<TypeExpr> = std::sync::LazyLock::new(|| {
    TypeExpr::Union(vec![
        TypeExpr::String,
        TypeExpr::Int,
        TypeExpr::Float,
        TypeExpr::Bool,
        TypeExpr::Duration,
    ])
});
static INT_REF_SINK: TypeExpr = TypeExpr::Int;

/// The declared type at one position in a [`Value`] tree.
///
/// Module boundaries use `TypeExpr`; provider resource attributes use
/// `AttributeType` plus their definition map. `Interpolation` constrains
/// expression parts to scalars that can be rendered to text regardless of the
/// outer container.
#[derive(Clone, Copy)]
pub(crate) enum RefSink<'a> {
    TypeExpr(&'a TypeExpr),
    EffectiveType {
        attr_type: &'a AttributeType,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        ancestry: crate::schema::FieldAncestry,
        owner: &'a crate::parser::SchemaTypeOwner,
    },
    Interpolation,
}

impl<'a> RefSink<'a> {
    fn expression_type(self) -> TypeExpr {
        match self {
            Self::EffectiveType {
                attr_type,
                defs,
                owner: crate::parser::SchemaTypeOwner::Provider(provider),
                ..
            } => expression_sink_type(attr_type, provider, defs),
            Self::Interpolation => INTERPOLATION_REF_SINK.clone(),
            Self::TypeExpr(ty) => ty.clone(),
        }
    }

    fn from_effective_type(
        effective: crate::schema::EffectiveType<'a>,
        owner: &'a crate::parser::SchemaTypeOwner,
    ) -> Self {
        Self::EffectiveType {
            attr_type: effective.as_type(),
            defs: effective.defs(),
            ancestry: effective.ancestry(),
            owner,
        }
    }

    fn as_type_expr(self) -> Option<&'a TypeExpr> {
        match self {
            Self::TypeExpr(type_expr) => Some(type_expr),
            Self::Interpolation => Some(&INTERPOLATION_REF_SINK),
            Self::EffectiveType {
                attr_type: _,
                defs: _,
                ancestry: _,
                owner: _,
            } => None,
        }
    }

    fn list_element(self) -> Option<Self> {
        match self {
            Self::TypeExpr(TypeExpr::List(inner)) => Some(Self::TypeExpr(inner)),
            Self::TypeExpr(TypeExpr::Union(members)) => {
                exactly_one(members.iter().filter_map(|member| match member {
                    TypeExpr::List(inner) => Some(inner.as_ref()),
                    TypeExpr::String
                    | TypeExpr::Bool
                    | TypeExpr::Int
                    | TypeExpr::Float
                    | TypeExpr::Duration
                    | TypeExpr::Simple(_)
                    | TypeExpr::UnloadedSchemaType(_)
                    | TypeExpr::Map(_)
                    | TypeExpr::Ref(_)
                    | TypeExpr::DottedUnresolved(_)
                    | TypeExpr::SchemaType { owned: _ }
                    | TypeExpr::Struct { fields: _ }
                    | TypeExpr::StringLiteral(_)
                    | TypeExpr::Union(_) => None,
                }))
                .map(Self::TypeExpr)
            }
            Self::EffectiveType {
                attr_type,
                defs,
                ancestry,
                owner,
            } => match attr_type.shape_with_defs(defs) {
                Shape::UnloadedSchemaType(_) => None,
                Shape::List {
                    element_type: inner,
                    ordered: _,
                    constraints: _,
                } => Some(Self::EffectiveType {
                    attr_type: inner,
                    defs,
                    ancestry,
                    owner,
                }),
                Shape::Union => {
                    unique_union_list_element(attr_type, defs).map(|inner| Self::EffectiveType {
                        attr_type: inner,
                        defs,
                        ancestry,
                        owner,
                    })
                }
                Shape::String {
                    identity_layer:
                        crate::schema::StringIdentityParts {
                            identity: _,
                            pattern: _,
                            length: _,
                            validate: _,
                            to_dsl: _,
                        },
                    member_layer: _,
                }
                | Shape::Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | Shape::Int {
                    identity_layer:
                        crate::schema::IntIdentityParts {
                            identity: _,
                            range: _,
                            validate: _,
                        },
                    member_layer: _,
                }
                | Shape::Float {
                    identity: _,
                    range: _,
                    validate: _,
                }
                | Shape::Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Shape::Map {
                    key: _,
                    value: _,
                    constraints: _,
                }
                | Shape::Struct { name: _ }
                | Shape::Bool
                | Shape::Duration => None,
            },
            Self::TypeExpr(
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::UnloadedSchemaType(_)
                | TypeExpr::Map(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { owned: _ }
                | TypeExpr::Struct { fields: _ }
                | TypeExpr::StringLiteral(_),
            )
            | Self::Interpolation => None,
        }
    }

    fn map_entry(self, key: &str) -> Option<Self> {
        match self {
            Self::TypeExpr(TypeExpr::Map(inner)) => Some(Self::TypeExpr(inner)),
            Self::TypeExpr(TypeExpr::Struct { fields }) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, field_type)| Self::TypeExpr(field_type)),
            Self::TypeExpr(TypeExpr::Union(members)) => {
                exactly_one(members.iter().filter(|member| {
                    matches!(member, TypeExpr::Map(_) | TypeExpr::Struct { fields: _ })
                }))
                .and_then(|member| Self::TypeExpr(member).map_entry(key))
            }
            Self::EffectiveType {
                attr_type,
                defs,
                ancestry,
                owner,
            } => match attr_type.shape_with_defs(defs) {
                Shape::UnloadedSchemaType(_) => None,
                Shape::Map {
                    value: inner,
                    key: _,
                    constraints: _,
                } => Some(Self::EffectiveType {
                    attr_type: inner,
                    defs,
                    ancestry,
                    owner,
                }),
                Shape::Struct { name: _ } => {
                    let fields = crate::schema::struct_fields_with_defs(attr_type, defs)
                        .expect("Shape::Struct must expose struct fields internally");
                    crate::schema::struct_field_for_key(fields, key, ancestry).map(|field| {
                        Self::EffectiveType {
                            attr_type: &field.field_type,
                            defs,
                            ancestry: ancestry.descend(field),
                            owner,
                        }
                    })
                }
                Shape::Union => unique_union_map_member(attr_type, defs).and_then(|member| {
                    Self::EffectiveType {
                        attr_type: member,
                        defs,
                        ancestry,
                        owner,
                    }
                    .map_entry(key)
                }),
                Shape::String {
                    identity_layer:
                        crate::schema::StringIdentityParts {
                            identity: _,
                            pattern: _,
                            length: _,
                            validate: _,
                            to_dsl: _,
                        },
                    member_layer: _,
                }
                | Shape::Reference {
                    identity: _,
                    pattern: _,
                    length: _,
                }
                | Shape::Int {
                    identity_layer:
                        crate::schema::IntIdentityParts {
                            identity: _,
                            range: _,
                            validate: _,
                        },
                    member_layer: _,
                }
                | Shape::Float {
                    identity: _,
                    range: _,
                    validate: _,
                }
                | Shape::Enum {
                    identity: _,
                    base: _,
                    values: _,
                    dsl_aliases: _,
                    validate: _,
                    to_dsl: _,
                }
                | Shape::List {
                    element_type: _,
                    ordered: _,
                    constraints: _,
                }
                | Shape::Bool
                | Shape::Duration => None,
            },
            Self::TypeExpr(
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::UnloadedSchemaType(_)
                | TypeExpr::List(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { owned: _ }
                | TypeExpr::StringLiteral(_),
            )
            | Self::Interpolation => None,
        }
    }
}

/// Failure from the single typed binding-versus-enum resolver.
///
/// Enum collisions retain their structured [`TypeError`] for LSP code actions;
/// pre-existing handle guidance remains text because that evaluator error is
/// intentionally parser-internal.
#[derive(Debug, Clone, PartialEq)]
pub enum BindingResolutionError {
    Type(TypeError),
    ValueType(Box<typecheck::TypeExprValueError>),
    ReferenceType(Box<BindingReferenceTypeError>),
    DeferredAccess(Box<DeferredAccessTypeError>),
    CustomType(Box<crate::parser::CustomTypeValidationError>),
    TypeResolution(StaticTypeResolutionError),
    Existing(String),
}

/// Typed incompatibility retained when a bare value binding crosses a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingReferenceTypeError {
    types: MismatchTypeNames,
    declared: Option<String>,
    path: ModuleCallDiagnosticPath,
    declared_constraint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModuleCallDiagnosticPath {
    Protected,
    Authored(ReferencePath<RelativeBindingName>),
    Expanded(ReferencePath<AbsoluteBindingAddress>),
}

impl ModuleCallDiagnosticPath {
    fn to_dot_string(&self) -> String {
        match self {
            Self::Protected => crate::schema::SECRET_VALUE_MASK.into(),
            Self::Authored(path) => path.to_dot_string(),
            Self::Expanded(path) => path.to_dot_string(),
        }
    }

    fn format_error(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
        call_label: &str,
        argument: &str,
        kind: &ModuleCallRefErrorKind,
    ) -> std::fmt::Result {
        match self {
            Self::Protected => format_module_call_ref_error_with_path(
                formatter,
                call_label,
                argument,
                crate::schema::SECRET_VALUE_MASK,
                kind,
            ),
            Self::Authored(path) => format_module_call_ref_error_with_path(
                formatter,
                call_label,
                argument,
                &path.to_dot_string(),
                kind,
            ),
            Self::Expanded(path) => format_module_call_ref_error_with_path(
                formatter,
                call_label,
                argument,
                &path.to_dot_string(),
                kind,
            ),
        }
    }
}

impl std::fmt::Display for BindingReferenceTypeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Type mismatch: {} ({})",
            self.types,
            match &self.declared {
                Some(declared) => {
                    format!("from {}, declared {declared}", self.path.to_dot_string())
                }
                None => format!("from {}", self.path.to_dot_string()),
            }
        )
    }
}

impl BindingResolutionError {
    pub fn type_error(&self) -> Option<&TypeError> {
        match self {
            Self::Type(error) => Some(error),
            Self::ValueType(_error) => None,
            Self::ReferenceType(_error) => None,
            Self::DeferredAccess(_error) => None,
            Self::CustomType(_error) => None,
            Self::TypeResolution(_error) => None,
            Self::Existing(_message) => None,
        }
    }
}

impl std::fmt::Display for BindingResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Type(error) => error.fmt(formatter),
            Self::ValueType(error) => error.fmt(formatter),
            Self::ReferenceType(error) => error.fmt(formatter),
            Self::DeferredAccess(error) => error.fmt(formatter),
            Self::CustomType(error) => error.fmt(formatter),
            Self::TypeResolution(error) => error.fmt(formatter),
            Self::Existing(message) => formatter.write_str(message),
        }
    }
}

// This is a terminal typed diagnostic: `Display` delegates to the contained
// error, so exposing that same error as a source would duplicate it in a chain
// renderer. Structured consumers use `type_error()` or match the public enum.
impl std::error::Error for BindingResolutionError {}

struct ExpectedEnumTarget<'a> {
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    owner: &'a crate::parser::SchemaTypeOwner,
}

/// Typed inputs shared by CLI and LSP while consuming parser-authored bare
/// binding uses. Provider context is deliberately mandatory: bootstrap parsing
/// may leave dotted declarations unresolved, but this validation seam may not.
pub struct ValidationTypeContext<'a, B: BindingAddress = RelativeBindingName> {
    file: &'a dyn BindingUseInput<B>,
    schemas: &'a SchemaRegistry,
    module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    provider_context: typecheck::TypecheckProviderContext<'a>,
    scope_bindings: BindingIndex<'a, B>,
    expression_report: &'a crate::parser::retained::ExpressionReport<B>,
}

/// Project the effective schema sink for authored expression judgement.
pub fn expression_sink_type(
    attr_type: &AttributeType,
    owner: &str,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> TypeExpr {
    project_expression_sink_type(attr_type, owner, defs, &mut Vec::new())
}

fn project_expression_sink_type<'a>(
    attr_type: &'a AttributeType,
    owner: &str,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    visited_refs: &mut Vec<&'a str>,
) -> TypeExpr {
    use crate::schema::RawShape;

    // Inference uses a String sentinel for union and Ref receivers. Admission
    // preserves alternatives and follows the owning schema's definitions.
    let restore_len = visited_refs.len();
    let Some(resolved) = attr_type.resolve_refs_for_union_walk_on_path(defs, visited_refs) else {
        // Retain inference's cycle cutoff rather than expanding recursive
        // schema definitions indefinitely.
        visited_refs.truncate(restore_len);
        return inference::attribute_type_to_type_expr(attr_type, owner);
    };
    let attr_type = resolved.as_attr();
    let projected = match attr_type.raw_shape() {
        RawShape::Union(members) => TypeExpr::Union(
            members
                .iter()
                .map(|member| project_expression_sink_type(member, owner, defs, visited_refs))
                .collect(),
        ),
        RawShape::List { element_type, .. } => TypeExpr::List(Box::new(
            project_expression_sink_type(element_type, owner, defs, visited_refs),
        )),
        RawShape::Map { value, .. } => TypeExpr::Map(Box::new(project_expression_sink_type(
            value,
            owner,
            defs,
            visited_refs,
        ))),
        RawShape::Struct { fields, .. } => TypeExpr::Struct {
            fields: fields
                .iter()
                .map(|field| {
                    (
                        field.name.clone(),
                        project_expression_sink_type(&field.field_type, owner, defs, visited_refs),
                    )
                })
                .collect(),
        },
        _ => inference::attribute_type_to_type_expr(attr_type, owner),
    };
    visited_refs.truncate(restore_len);
    projected
}

impl<'a, B: BindingAddress> ValidationTypeContext<'a, B> {
    pub fn in_session<E, D>(
        file: &'a crate::parser::retained::JudgedFile<'_, E, D, B>,
        session: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::with_provider_context(
            file,
            typecheck::TypecheckProviderContext::for_session(session),
            module_signatures,
        )
    }

    pub fn new<E, D>(
        file: &'a crate::parser::retained::JudgedFile<'_, E, D, B>,
        provider_context: &'a LoadedProviderContext,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::with_provider_context(
            file,
            typecheck::TypecheckProviderContext::loaded(provider_context),
            module_signatures,
        )
    }

    /// Build the validation view used by bootstrap/editor callers that have
    /// an incomplete provider custom-type catalog. Structural checks and
    /// successfully loaded owners remain strict; unavailable owners defer.
    pub fn new_providerless<E, D>(
        file: &'a crate::parser::retained::JudgedFile<'_, E, D, B>,
        provider_context: &'a ProviderContext,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::with_provider_context(
            file,
            typecheck::TypecheckProviderContext::providerless(provider_context),
            module_signatures,
        )
    }

    fn with_provider_context<E, D>(
        judged: &'a crate::parser::retained::JudgedFile<'_, E, D, B>,
        provider_context: typecheck::TypecheckProviderContext<'a>,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let file = judged.file();
        let schemas = judged.schemas();
        Self {
            file,
            schemas,
            provider_context,
            module_signatures,
            scope_bindings: match provider_context.session() {
                Some(session) => {
                    BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(
                        file,
                        schemas,
                        None,
                        module_signatures,
                        session,
                    )
                }
                None => BindingIndex::from_parsed_with_module_signatures(
                    file,
                    schemas,
                    module_signatures,
                    provider_context.as_provider_context(),
                ),
            },
            expression_report: judged.report(),
        }
    }

    fn raw_provider_context(&self) -> &'a ProviderContext {
        self.provider_context.as_provider_context()
    }
}

/// Shared location-free unused-resource-binding diagnostic.
pub fn unused_binding_message(binding: &str) -> String {
    format!("Unused let binding '{binding}'. Consider using an anonymous resource instead.")
}

/// One typed failure tied directly to the parser token that authored the use.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingUseFinding {
    pub use_site: crate::parser::BareBindingUse,
    pub error: BindingResolutionError,
}

impl BindingUseFinding {
    pub fn diagnostic(&self) -> crate::parser::Located<&Self> {
        crate::parser::Located::new(self.use_site.location().clone(), self)
    }

    fn error(use_site: crate::parser::BareBindingUse, error: BindingResolutionError) -> Self {
        Self { use_site, error }
    }
}

impl std::fmt::Display for BindingUseFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source_file = crate::utils::normalize_source_path(self.use_site.source_file.path());
        write!(
            formatter,
            "{} at line {}",
            source_file.display(),
            self.use_site.span.start_line(),
        )
    }
}

impl std::error::Error for BindingUseFinding {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Complete result of consuming one parsed file's out-of-band provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingUseReport {
    findings: Vec<BindingUseFinding>,
    expression_owned_uses: Vec<crate::parser::BareBindingUse>,
}

impl std::fmt::Display for BindingUseReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, finding) in self.findings.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            let source_file =
                crate::utils::normalize_source_path(finding.use_site.source_file.path());
            write!(
                formatter,
                "{}: {} at line {}",
                source_file.display(),
                finding.error,
                finding.use_site.span.start_line(),
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for BindingUseReport {}

/// The exact parsed file whose bare binding uses were resolved successfully.
///
/// This owner is deliberately neither `Clone` nor `Copy`: validation readiness
/// follows the checked file instead of being a detachable proof token.
///
/// ```compile_fail,E0382
/// use carina_core::parser::InferredExportParam;
/// use carina_core::validation::ResolvedFile;
///
/// fn duplicate(resolved: ResolvedFile<InferredExportParam>) {
///     let first = resolved;
///     let second = resolved;
///     drop((first, second));
/// }
/// ```
#[derive(Debug)]
pub struct ResolvedFile<E> {
    inner: crate::parser::ExpandedFile<E>,
}

impl<E> ResolvedFile<E> {
    /// Admit the complete file produced by binding resolution and expansion.
    pub fn provider_plan(&self) -> crate::provider::ProviderPlan {
        self.inner.provider_plan()
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn for_test(inner: crate::parser::ExpandedFile<E>) -> Self {
        Self { inner }
    }

    pub fn as_file(&self) -> &crate::parser::ExpandedFile<E> {
        &self.inner
    }

    pub fn into_file(self) -> crate::parser::ExpandedFile<E> {
        self.inner
    }
}

impl<E> std::ops::Deref for ResolvedFile<E> {
    type Target = crate::parser::ExpandedFile<E>;

    fn deref(&self) -> &Self::Target {
        self.as_file()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveFileError {
    #[error(transparent)]
    Unjudged(#[from] crate::parser::retained::UnjudgedConfiguration),
    #[error(transparent)]
    Expression(Box<crate::parser::Located<inference::InferenceError>>),
    #[error(transparent)]
    BindingUse(#[from] BindingUseReport),
    #[error(transparent)]
    Expansion(#[from] crate::resource::ExpansionError),
}

impl BindingUseReport {
    pub fn findings(&self) -> &[BindingUseFinding] {
        &self.findings
    }

    /// Findings whose sink remains owned by resource validation adapters.
    /// Module-argument findings are consumed by [`check_module_calls`].
    pub fn resource_findings(&self) -> impl Iterator<Item = &BindingUseFinding> {
        self.findings.iter().filter(|finding| {
            matches!(
                &finding.use_site.owner,
                crate::parser::BindingUseOwner::Resource(_address)
            )
        })
    }

    pub fn has_resource_findings(&self) -> bool {
        self.resource_findings().next().is_some()
    }

    pub fn into_findings(self) -> Vec<BindingUseFinding> {
        self.findings
    }

    pub fn is_ready(&self) -> bool {
        self.findings.is_empty()
    }
}

fn binding_value_assignable_to_enum<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    declared_type: Option<&TypeExpr>,
    provider_context: typecheck::TypecheckProviderContext<'_>,
    target: &ExpectedEnumTarget<'_>,
    resolver: &EnumValueResolver<'_, crate::schema::Compiled>,
    identity: &TypeIdentity,
) -> Result<(), BindingResolutionError> {
    let config = provider_context.as_provider_context();
    let scalar_kind = crate::parser::IdentifiedCustomTypeDeclaration::new(
        target
            .attr_type
            .resolve_refs_with_defs(target.defs)
            .as_attr(),
    )
    .expect("an enum target has an admitted scalar declaration")
    .scalar_kind();
    let identity_assignable = |source: &AttributeType| -> Result<bool, BindingResolutionError> {
        let target_type = scalar_kind.identified_type(identity.clone());
        let sink = target_type
            .in_schema(crate::schema::empty_defs_for_schema_walks())
            .with_authority(
                &config.custom_types,
                crate::schema::SinkAuthority::Owner(target.owner),
            );
        let assignment = crate::schema::TypeInSchema::schemaless(source).assignment_to(sink);
        if config.custom_types_loaded() {
            assignment
                .require_loaded()
                .map_err(|error| BindingResolutionError::CustomType(Box::new(error)))
        } else {
            match provider_context.session() {
                Some(session) => Ok(assignment.record_unchecked(session)),
                None => assignment
                    .require_loaded()
                    .map_err(|error| BindingResolutionError::CustomType(Box::new(error))),
            }
        }
    };
    if let Some(declared_type) = declared_type {
        let resolved = resolve_type_expr(declared_type, config)
            .map_err(BindingResolutionError::TypeResolution)?;
        if matches!(
            &resolved,
            TypeExpr::UnloadedSchemaType(_) | TypeExpr::SchemaType { owned: _ }
        ) {
            let source = lift_type_expr(&resolved).expect("nominal enum evidence lifts");
            if identity_assignable(&source)? {
                return Ok(());
            }
        }
        let crate::parser::SchemaTypeOwner::Provider(provider) = target.owner;
        let expected = TypeExpr::SchemaType {
            owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
                provider,
                target
                    .attr_type
                    .resolve_refs_with_defs(target.defs)
                    .as_attr(),
            )
            .expect("an enum target has an admitted scalar declaration"),
        };
        return Err(BindingResolutionError::Type(TypeError::TypeMismatch {
            types: MismatchTypeNames::for_type_exprs(&expected, &resolved),
        }));
    }
    match value {
        GenericValue::Concrete(crate::resource::ConcreteValue::CanonicalEnum(canonical)) => {
            let source = scalar_kind.identified_type(canonical.identity().clone());
            if !identity_assignable(&source)? {
                return Err(BindingResolutionError::Type(TypeError::TypeMismatch {
                    types: MismatchTypeNames::for_types(
                        target.attr_type.in_schema(target.defs),
                        crate::schema::TypeInSchema::schemaless(&source),
                    ),
                }));
            }
            resolver
                .resolve_state_text(canonical.api_value())
                .map(|_| ())
                .map_err(BindingResolutionError::Type)
        }
        GenericValue::Concrete(crate::resource::ConcreteValue::EnumIdentifier(raw)) => resolver
            .resolve_raw(raw)
            .map(|_| ())
            .map_err(BindingResolutionError::Type),
        GenericValue::Concrete(
            crate::resource::ConcreteValue::String(_)
            | crate::resource::ConcreteValue::Int(_)
            | crate::resource::ConcreteValue::Float(_)
            | crate::resource::ConcreteValue::Bool(_)
            | crate::resource::ConcreteValue::Duration(_)
            | crate::resource::ConcreteValue::List(_)
            | crate::resource::ConcreteValue::StringList(_)
            | crate::resource::ConcreteValue::Map(_),
        )
        | GenericValue::Deferred(
            crate::resource::DeferredValue::ModuleIf(_)
            | crate::resource::DeferredValue::ResourceRef { path: _ }
            | crate::resource::DeferredValue::BindingRef { binding: _ }
            | crate::resource::DeferredValue::Coalesce {
                value: _,
                default: _,
            }
            | crate::resource::DeferredValue::IntegerArithmetic(_)
            | crate::resource::DeferredValue::Interpolation(_)
            | crate::resource::DeferredValue::FunctionCall {
                name: _,
                args: _,
                source_line: _,
            }
            | crate::resource::DeferredValue::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line: _,
            }
            | crate::resource::DeferredValue::ResolvedFunctionCall {
                function: _,
                functions: _,
                args: _,
                source_line: _,
            }
            | crate::resource::DeferredValue::Access(_)
            | crate::resource::DeferredValue::Secret(_)
            | crate::resource::DeferredValue::Unknown(_)
            | crate::resource::DeferredValue::Placeholder(_),
        ) => crate::schema::validate_attr_with_defs(
            target.attr_type,
            value,
            target.defs,
            target.ancestry,
        )
        .map_err(BindingResolutionError::Type),
    }
}

fn enum_shadow_or_existing(
    use_site: &crate::parser::BareBindingUse,
    resolver: &EnumValueResolver<'_, crate::schema::Compiled>,
    existing_error: BindingResolutionError,
) -> BindingResolutionError {
    let Some(identity) = resolver.enum_identity() else {
        return existing_error;
    };
    match resolver.resolve_member(&RawEnumIdentifier::parse(use_site.name.clone())) {
        Ok((_canonical, qualified)) if qualified.to_string() != use_site.name => {
            BindingResolutionError::Type(TypeError::BindingShadowsEnumMember(Box::new(
                BindingShadowsEnumMemberError {
                    name: use_site.name.clone(),
                    binding_kind: match &use_site.binding {
                        crate::parser::BareBindingKind::Value { declaration } => *declaration,
                        crate::parser::BareBindingKind::Structural { .. } => BindingKind::Let,
                    },
                    r#enum: identity,
                    qualified,
                },
            )))
        }
        Ok(_) | Err(_) => existing_error,
    }
}

fn enum_target_from_sink<'a>(sink: RefSink<'a>) -> Option<ExpectedEnumTarget<'a>> {
    match sink {
        RefSink::EffectiveType {
            attr_type,
            defs,
            ancestry,
            owner,
        } if EnumValueResolver::with_defs(attr_type, defs)
            .enum_identity()
            .is_some() =>
        {
            Some(ExpectedEnumTarget {
                attr_type,
                defs,
                ancestry,
                owner,
            })
        }
        RefSink::EffectiveType {
            attr_type: _,
            defs: _,
            ancestry: _,
            owner: _,
        }
        | RefSink::TypeExpr(_)
        | RefSink::Interpolation => None,
    }
}

fn declared_value_assignable_to_sink<B: BindingAddress>(
    name: &str,
    declared_type: &TypeExpr,
    value: &GenericValue<ParserPlaceholder, B>,
    sink: RefSink<'_>,
    provider_context: typecheck::TypecheckProviderContext<'_>,
    schemas: &SchemaRegistry,
) -> Result<(), BindingResolutionError> {
    let config = provider_context.as_provider_context();
    let resolved =
        resolve_type_expr(declared_type, config).map_err(BindingResolutionError::TypeResolution)?;
    let resolved = crate::parser::resolve_type_expr_from_schemas(&resolved, schemas, config)
        .map_err(BindingResolutionError::TypeResolution)?;
    let Some(source) = lift_type_expr(&resolved) else {
        return Ok(());
    };
    let source = crate::schema::TypeInSchema::schemaless(&source);
    let interpolation_sink =
        lift_type_expr(&INTERPOLATION_REF_SINK).expect("built-in interpolation operand types lift");
    let lifted_sink;
    let target = match sink {
        RefSink::EffectiveType {
            attr_type,
            defs,
            ancestry: _,
            owner,
        } => attr_type.in_schema(defs).with_authority(
            &config.custom_types,
            crate::schema::SinkAuthority::Owner(owner),
        ),
        RefSink::Interpolation => interpolation_sink.assignment_sink(&config.custom_types),
        RefSink::TypeExpr(type_expr) => {
            let resolved = resolve_type_expr(type_expr, config)
                .and_then(|resolved| {
                    crate::parser::resolve_type_expr_from_schemas(&resolved, schemas, config)
                })
                .map_err(BindingResolutionError::TypeResolution)?;
            let Some(lifted) = lift_type_expr(&resolved) else {
                return Ok(());
            };
            lifted_sink = lifted;
            lifted_sink.assignment_sink(&config.custom_types)
        }
    };
    let assignment = source.assignment_to(target);
    let assignable = if config.custom_types_loaded() {
        assignment
            .require_loaded()
            .map_err(|error| BindingResolutionError::CustomType(Box::new(error)))?
    } else {
        match provider_context.session() {
            Some(session) => assignment.record_unchecked(session),
            None => assignment
                .require_loaded()
                .map_err(|error| BindingResolutionError::CustomType(Box::new(error)))?,
        }
    };
    if assignable {
        // Declaration compatibility does not validate a written value. The
        // destination owns this check, using its complete original declaration.
        let ancestry = match sink {
            RefSink::EffectiveType { ancestry, .. } => ancestry,
            RefSink::TypeExpr(_) | RefSink::Interpolation => FieldAncestry::WRITABLE,
        };
        return crate::schema::validate_attr_with_defs(
            target.as_type(),
            value,
            target.defs(),
            ancestry,
        )
        .map_err(BindingResolutionError::Type);
    }
    // A declared source is still a reference mismatch. Retain its typed
    // identity so module-call diagnostics can own it and cover the resolver's
    // backstop, just as they do for inferred reference types. The spelling is
    // lexical provenance, even after the caller's value has been substituted.
    Err(BindingResolutionError::ReferenceType(Box::new(
        BindingReferenceTypeError {
            types: MismatchTypeNames::for_types(*target, source),
            declared: None,
            path: ModuleCallDiagnosticPath::Authored(ReferencePath::Binding(
                RelativeBindingName::new(name),
            )),
            declared_constraint: matches!(
                sink,
                RefSink::TypeExpr(TypeExpr::Union(_) | TypeExpr::StringLiteral(_))
            ),
        },
    )))
}

struct ValueSource<'a, B: BindingAddress> {
    value: &'a GenericValue<ParserPlaceholder, B>,
    declared_type: Option<&'a TypeExpr>,
}

fn reference_value_mismatch<B: BindingAddress>(
    reference: ReferencePathRef<'_, B>,
    declared_type: Option<&TypeExpr>,
    target: crate::schema::AssignmentSink<'_, crate::schema::Compiled>,
    bindings: &BindingIndex<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Option<BindingResolutionError> {
    match bindings.ref_type(reference) {
        RefType::Typed(source) => {
            let checked = match declared_type
                .and_then(|declared| source.clone().with_value_declaration(declared, bindings))
            {
                Some(source) => source.assignment_to_sink(target, bindings),
                None => crate::binding_index::DeclaredValueAssignment::from_result(
                    source.is_assignable_to_sink(target, bindings),
                ),
            };
            match checked {
                crate::binding_index::DeclaredValueAssignment::Assignable
                | crate::binding_index::DeclaredValueAssignment::IncomingBoundaryMismatch => None,
                crate::binding_index::DeclaredValueAssignment::Rejected(
                    RefTypeAssignabilityError::Mismatch(mismatch),
                ) => Some(BindingResolutionError::ReferenceType(Box::new(
                    BindingReferenceTypeError {
                        types: mismatch.type_names().clone(),
                        declared: mismatch.declared_type_name().map(str::to_string),
                        path: diagnostic_path(reference),
                        declared_constraint: false,
                    },
                ))),
                crate::binding_index::DeclaredValueAssignment::Rejected(
                    RefTypeAssignabilityError::Unchecked(reason),
                ) => Some(BindingResolutionError::CustomType(reason)),
            }
        }
        RefType::Unchecked => None,
        RefType::UnknownBinding { binding, attribute } => {
            let reference = if attribute.is_empty() {
                binding
            } else {
                format!("{binding}.{attribute}")
            };
            Some(BindingResolutionError::Existing(format!(
                "unknown binding in reference '{reference}'"
            )))
        }
        RefType::UnknownAttribute(error) => {
            Some(BindingResolutionError::Existing(error.to_string()))
        }
    }
}

fn deferred_value_mismatch<B: BindingAddress>(
    source: ValueSource<'_, B>,
    expression: &crate::parser::retained::JudgedExpr,
    target: crate::schema::AssignmentSink<'_, crate::schema::Compiled>,
    schemas: &SchemaRegistry,
    bindings: &BindingIndex<'_, B>,
    authored_binding: &str,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Option<BindingResolutionError> {
    let value = source.value;
    match value {
        GenericValue::Deferred(crate::resource::DeferredValue::ModuleIf(deferred)) => {
            deferred.values()[1..].iter().find_map(|branch| {
                deferred_value_mismatch(
                    ValueSource {
                        value: branch,
                        declared_type: source.declared_type,
                    },
                    expression,
                    target,
                    schemas,
                    bindings,
                    authored_binding,
                    diagnostic_path,
                )
            })
        }
        GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path }) => {
            reference_value_mismatch(
                ReferencePathRef::Access(path),
                source.declared_type,
                target,
                bindings,
                diagnostic_path,
            )
        }
        GenericValue::Deferred(crate::resource::DeferredValue::BindingRef { binding }) => {
            reference_value_mismatch(
                ReferencePathRef::Binding(binding),
                source.declared_type,
                target,
                bindings,
                diagnostic_path,
            )
        }
        GenericValue::Deferred(
            crate::resource::DeferredValue::Coalesce {
                value: _,
                default: _,
            }
            | crate::resource::DeferredValue::IntegerArithmetic(_)
            | crate::resource::DeferredValue::Interpolation(_)
            | crate::resource::DeferredValue::FunctionCall {
                name: _,
                args: _,
                source_line: _,
            }
            | crate::resource::DeferredValue::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line: _,
            }
            | crate::resource::DeferredValue::ResolvedFunctionCall {
                function: _,
                functions: _,
                args: _,
                source_line: _,
            }
            | crate::resource::DeferredValue::Access(_)
            | crate::resource::DeferredValue::Secret(_)
            | crate::resource::DeferredValue::Unknown(_)
            | crate::resource::DeferredValue::Placeholder(_),
        ) => check_deferred_access_type(value, target, expression, schemas, bindings)
            .err()
            .map(|error| match error {
                DeferredAccessTypeError::TypeMismatch {
                    types,
                    origin: _origin,
                } => {
                    // Inference describes a parser-internal unresolved-module
                    // placeholder here. The authored binding path is the
                    // stable source used for the shared diagnostic and dedupe.
                    BindingResolutionError::ReferenceType(Box::new(BindingReferenceTypeError {
                        types,
                        declared: None,
                        path: ModuleCallDiagnosticPath::Authored(ReferencePath::Binding(
                            RelativeBindingName::new(authored_binding.to_string()),
                        )),
                        declared_constraint: false,
                    }))
                }
                DeferredAccessTypeError::InvalidAccess { error } => {
                    BindingResolutionError::DeferredAccess(Box::new(
                        DeferredAccessTypeError::InvalidAccess { error },
                    ))
                }
            }),
        GenericValue::Concrete(
            crate::resource::ConcreteValue::String(_)
            | crate::resource::ConcreteValue::EnumIdentifier(_)
            | crate::resource::ConcreteValue::CanonicalEnum(_)
            | crate::resource::ConcreteValue::Int(_)
            | crate::resource::ConcreteValue::Float(_)
            | crate::resource::ConcreteValue::Bool(_)
            | crate::resource::ConcreteValue::Duration(_)
            | crate::resource::ConcreteValue::List(_)
            | crate::resource::ConcreteValue::StringList(_)
            | crate::resource::ConcreteValue::Map(_),
        ) => None,
    }
}

fn binding_use_expression<'a, B: BindingAddress>(
    use_site: &crate::parser::BareBindingUse,
    context: &'a ValidationTypeContext<'_, B>,
) -> Result<&'a crate::parser::retained::JudgedExpr, crate::parser::retained::MissingEvidence> {
    let report = match &use_site.owner {
        crate::parser::BindingUseOwner::Resource(address) => context
            .expression_report
            .for_resource(address.resource_id()),
        crate::parser::BindingUseOwner::ModuleArgument { .. } => Ok(context.expression_report),
    }?;
    report.expression_in(&use_site.expression_instance, use_site.location())
}

fn precheck_value_source<B: BindingAddress>(
    use_site: &crate::parser::BareBindingUse,
    value: &GenericValue<ParserPlaceholder, B>,
    declared_type: Option<&TypeExpr>,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Option<BindingResolutionError> {
    let expression = match binding_use_expression(use_site, context) {
        Ok(expression) => expression,
        Err(error) => {
            return Some(BindingResolutionError::DeferredAccess(Box::new(
                DeferredAccessTypeError::InvalidAccess {
                    error: error.into(),
                },
            )));
        }
    };
    let expected = sink.expression_type();
    let admission =
        crate::parser::retained::admit_expression(expression, &expected).map_err(|error| {
            BindingResolutionError::DeferredAccess(Box::new(
                DeferredAccessTypeError::InvalidAccess { error },
            ))
        });
    if matches!(
        admission,
        Ok(crate::parser::retained::ExpressionAdmission::DeferredInterpolation)
    ) {
        return None;
    }
    let interpolation_sink =
        lift_type_expr(&INTERPOLATION_REF_SINK).expect("built-in interpolation operand types lift");
    let lifted_sink;
    let target = match sink {
        RefSink::EffectiveType {
            attr_type,
            defs,
            ancestry: _,
            owner,
        } => attr_type.in_schema(defs).with_authority(
            &context.raw_provider_context().custom_types,
            crate::schema::SinkAuthority::Owner(owner),
        ),
        RefSink::Interpolation => {
            interpolation_sink.assignment_sink(&context.raw_provider_context().custom_types)
        }
        RefSink::TypeExpr(type_expr) => {
            let resolved = match context.scope_bindings.resolve_declared_type(type_expr) {
                Ok(resolved) => resolved,
                Err(error) => return Some(BindingResolutionError::TypeResolution(error)),
            };
            let Some(lifted) = lift_type_expr(&resolved) else {
                return admission.err();
            };
            lifted_sink = lifted;
            lifted_sink.assignment_sink(&context.raw_provider_context().custom_types)
        }
    };
    let diagnostic = deferred_value_mismatch(
        ValueSource {
            value,
            declared_type,
        },
        expression,
        target,
        context.schemas,
        &context.scope_bindings,
        &use_site.name,
        diagnostic_path,
    );
    match admission {
        // A projection can refine a type mismatch's wording, but can never
        // overturn rejected admission, including when it folded to a valid value.
        Err(error) => Some(if expression.assignment_source_type().is_some() {
            diagnostic
                .or_else(|| {
                    binding_value_constraints(use_site, value, declared_type, sink, context).err()
                })
                .unwrap_or(error)
        } else {
            error
        }),
        Ok(_) => diagnostic,
    }
}

fn resolve_value_source_against_sink<B: BindingAddress>(
    use_site: &crate::parser::BareBindingUse,
    value: &GenericValue<ParserPlaceholder, B>,
    declared_type: Option<&TypeExpr>,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Result<(), BindingResolutionError> {
    let expected = sink.expression_type();
    let builtin_members = binding_use_expression(use_site, context).is_ok_and(|expression| {
        expression
            .for_sink(&expected, context.raw_provider_context())
            .has_builtin_constructed_members()
    });
    // Builtin bootstrap types describe the result, not a nominal declaration.
    // Check its evaluated members through main's existing binding-value path.
    let evaluated = builtin_members
        .then(|| context.file.value_at_use(use_site))
        .flatten();
    let value = evaluated.as_ref().unwrap_or(value);
    let declared_type = if builtin_members { None } else { declared_type };
    let mut prechecked = precheck_value_source(
        use_site,
        value,
        declared_type,
        sink,
        context,
        diagnostic_path,
    );
    if let Some(BindingResolutionError::ReferenceType(error)) = &mut prechecked {
        error.declared_constraint = declared_type.is_some()
            && matches!(
                sink,
                RefSink::TypeExpr(TypeExpr::Union(_) | TypeExpr::StringLiteral(_))
            );
    }

    let result = match prechecked {
        Some(error) => Err(error),
        None => binding_value_constraints(use_site, value, declared_type, sink, context),
    };
    match enum_target_from_sink(sink) {
        Some(target) => {
            let resolver = EnumValueResolver::with_defs(target.attr_type, target.defs);
            result.map_err(|error| enum_shadow_or_existing(use_site, &resolver, error))
        }
        None => result,
    }
}

fn binding_value_constraints<B: BindingAddress>(
    use_site: &crate::parser::BareBindingUse,
    value: &GenericValue<ParserPlaceholder, B>,
    declared_type: Option<&TypeExpr>,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_, B>,
) -> Result<(), BindingResolutionError> {
    if let Some(target) = enum_target_from_sink(sink) {
        let resolver = EnumValueResolver::with_defs(target.attr_type, target.defs);
        let identity = resolver
            .enum_identity()
            .expect("enum_target_from_sink proved the enum identity");
        return binding_value_assignable_to_enum(
            value,
            declared_type,
            context.provider_context,
            &target,
            &resolver,
            &identity,
        );
    }
    if let Some(declared_type) = declared_type {
        return declared_value_assignable_to_sink(
            &use_site.name,
            declared_type,
            value,
            sink,
            context.provider_context,
            context.schemas,
        );
    }

    match sink {
        RefSink::EffectiveType {
            attr_type,
            defs,
            ancestry,
            owner: _,
        } => crate::schema::validate_attr_with_defs(attr_type, value, defs, ancestry)
            .map_err(BindingResolutionError::Type),
        RefSink::Interpolation => crate::schema::validate_attr_with_defs(
            &lift_type_expr(&INTERPOLATION_REF_SINK)
                .expect("built-in interpolation operand types lift"),
            value,
            crate::schema::empty_defs_for_schema_walks(),
            FieldAncestry::WRITABLE,
        )
        .map_err(BindingResolutionError::Type),
        RefSink::TypeExpr(type_expr) => {
            let expression = context
                .expression_report
                .expression_in(&use_site.expression_instance, use_site.location())
                .map_err(|error| {
                    BindingResolutionError::ValueType(Box::new(
                        typecheck::TypeExprValueError::InvalidAccess {
                            error: error.into(),
                        },
                    ))
                })?;
            context
                .provider_context
                .check_at(
                    type_expr,
                    value,
                    expression,
                    deferred_types::ProviderTypeCheckSite::new(
                        use_site.location().clone(),
                        format!("binding '{}'", use_site.name),
                    ),
                )
                .map_err(|error| BindingResolutionError::ValueType(Box::new(error)))
        }
    }
}

fn resource_sink<
    'a,
    E,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    file: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    address: &crate::parser::ResourceAddress,
    path: &crate::parser::AttributePath,
    schemas: &'a SchemaRegistry,
) -> Option<RefSink<'a>> {
    let schema = match file
        .iter_all_resources()
        .find(|resource| resource.id() == address.resource_id())
    {
        Some(ResourceRef::LiveComposition(_)) => return None,
        Some(ResourceRef::DataSource(data_source)) => schemas.get_for_data_source(data_source)?,
        Some(ResourceRef::Resource(resource)) => schemas.get_for(resource)?,
        Some(ResourceRef::Deferred {
            resource,
            deferred: _deferred,
        }) => schemas.get_for(resource.as_resource())?,
        None => return None,
    };
    let mut segments = path.segments().iter();
    let crate::parser::AttributePathSegment::Attribute(root) = segments.next()? else {
        return None;
    };
    let mut sink = RefSink::from_effective_type(
        schema
            .schema()
            .effective_attribute_by_authored_name(root)?
            .value_type(),
        schema.schema_type_owner(),
    );
    for segment in segments {
        sink = match segment {
            crate::parser::AttributePathSegment::ListIndex(_) => sink.list_element()?,
            crate::parser::AttributePathSegment::Key(key)
            | crate::parser::AttributePathSegment::StructField(key) => sink.map_entry(key)?,
            crate::parser::AttributePathSegment::Attribute(_) => return None,
            crate::parser::AttributePathSegment::ProtectedKey { key, kind } => {
                if *kind == crate::parser::ProtectedPathKind::Attribute {
                    return None;
                }
                sink.map_entry(key.transport_identity())?
            }
        };
    }
    Some(sink)
}

fn module_argument_type<E, D, B: BindingAddress>(
    file: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    call_id: &crate::parser::ModuleCallId,
    argument: &str,
    signatures: &crate::module_resolver::ResolvedModuleSignatures,
) -> Option<TypeExpr> {
    if let Some(call) = file.module_calls.iter().find(|call| call.id() == call_id) {
        let static_type = &signatures
            .get(&call.module_name)?
            .arguments
            .iter()
            .find(|parameter| parameter.name == argument)
            .map(|parameter| &parameter.static_type)?;
        return static_type.known().cloned();
    }
    file.compositions
        .iter()
        .find(|composition| {
            composition
                .call_path()
                .calls()
                .any(|call| &call.id == call_id)
        })
        .and_then(|composition| composition.signature.arguments.get(argument))
        .and_then(|argument| argument.declared_type().known().cloned())
}

fn module_enum_declaration(
    expected: &TypeExpr,
    config: typecheck::TypecheckProviderContext<'_>,
) -> Result<Option<AttributeType>, BindingResolutionError> {
    let TypeExpr::SchemaType { owned } = expected else {
        return Ok(None);
    };
    let Some(config) = config.loaded_context() else {
        return Ok(None);
    };
    config
        .enum_declaration(owned)
        .map_err(|error| BindingResolutionError::CustomType(Box::new(error)))
}

fn descend_module_type_expr(
    expected: &TypeExpr,
    segment: &crate::parser::AttributePathSegment,
) -> Option<TypeExpr> {
    use crate::parser::AttributePathSegment;

    match expected {
        TypeExpr::List(element) => match segment {
            AttributePathSegment::ListIndex(_) => Some((**element).clone()),
            AttributePathSegment::Attribute(_)
            | AttributePathSegment::Key(_)
            | AttributePathSegment::StructField(_)
            | AttributePathSegment::ProtectedKey { .. } => None,
        },
        TypeExpr::Map(element) => match segment {
            AttributePathSegment::Key(_) | AttributePathSegment::StructField(_) => {
                Some((**element).clone())
            }
            AttributePathSegment::Attribute(_) | AttributePathSegment::ListIndex(_) => None,
            AttributePathSegment::ProtectedKey { kind, .. } => {
                (*kind != crate::parser::ProtectedPathKind::Attribute).then(|| (**element).clone())
            }
        },
        TypeExpr::Struct { fields } => match segment {
            AttributePathSegment::Key(name) | AttributePathSegment::StructField(name) => fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, field_type)| field_type.clone()),
            AttributePathSegment::Attribute(_) | AttributePathSegment::ListIndex(_) => None,
            AttributePathSegment::ProtectedKey { kind, key } => {
                if *kind == crate::parser::ProtectedPathKind::Attribute {
                    None
                } else {
                    fields
                        .iter()
                        .find(|(field, _)| key.matches(field))
                        .map(|(_, ty)| ty.clone())
                }
            }
        },
        TypeExpr::Union(members) => exactly_one(
            members
                .iter()
                .filter_map(|member| descend_module_type_expr(member, segment)),
        ),
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { owned: _ }
        | TypeExpr::StringLiteral(_) => None,
    }
}

fn resolve_module_argument_path_type(
    expected: &TypeExpr,
    path: &crate::parser::AttributePath,
    config: &ProviderContext,
) -> Result<Option<ResolvedParameterType>, BindingResolutionError> {
    let Some(resolved) = resolve_parameter_type_for_check(expected, config)
        .map_err(BindingResolutionError::TypeResolution)?
    else {
        return Ok(None);
    };
    let mut segments = path.segments().iter();
    let Some(crate::parser::AttributePathSegment::Attribute(_)) = segments.next() else {
        return Ok(None);
    };
    let mut leaf = resolved.as_type_expr();
    for segment in segments {
        let Some(descended) = descend_module_type_expr(&leaf, segment) else {
            return Ok(None);
        };
        leaf = descended;
    }
    Ok(Some(match leaf {
        TypeExpr::Ref(path) => ResolvedParameterType::Handle(path),
        value @ (TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { owned: _ }
        | TypeExpr::Struct { fields: _ }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Union(_)) => ResolvedParameterType::Value(value),
    }))
}

fn authored_bare_handle_error<B: BindingAddress>(
    record: &crate::parser::ParsedBareBindingUse<B>,
    handle: &crate::eval_value::UnexpandedStructuralHandle<B>,
) -> crate::eval_value::BareStructuralHandle {
    let mut error = handle.bare_error();
    if let crate::parser::BareBindingKind::Structural { kind, .. } = &record.authored.binding {
        error.kind = kind.clone();
    }
    error
}

fn resolve_record_against_module<B: BindingAddress>(
    record: &crate::parser::ParsedBareBindingUse<B>,
    expected: &TypeExpr,
    context: &ValidationTypeContext<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Result<(), BindingResolutionError> {
    let Some(resolved) = resolve_module_argument_path_type(
        expected,
        &record.authored.path,
        context.raw_provider_context(),
    )?
    else {
        return Ok(());
    };
    let expected = resolved.as_type_expr();
    let enum_declaration = module_enum_declaration(&expected, context.provider_context)?;

    let result = match &record.source {
        crate::parser::BareBindingSource::Value {
            value,
            declared_type,
        } => resolve_value_source_against_sink(
            &record.authored,
            value,
            declared_type.as_deref(),
            RefSink::TypeExpr(&expected),
            context,
            diagnostic_path,
        ),
        crate::parser::BareBindingSource::Structural(handle) => {
            check_handle_sink(handle, &resolved)
                .map_err(|error| BindingResolutionError::Existing(error.to_string()))
        }
    };
    result.map_err(|error| match enum_declaration.as_ref() {
        Some(declaration) => {
            let resolver = EnumValueResolver::new(declaration);
            enum_shadow_or_existing(&record.authored, &resolver, error)
        }
        None => error,
    })
}

fn same_authored_finding(left: &BindingUseFinding, right: &BindingUseFinding) -> bool {
    same_authored_use(&left.use_site, &right.use_site) && left.error == right.error
}

fn same_authored_use(
    left: &crate::parser::BareBindingUse,
    right: &crate::parser::BareBindingUse,
) -> bool {
    left.source_file == right.source_file && left.span == right.span && left.name == right.name
}

/// Whether the typed binding-use authority owns validation of an authored
/// module argument. A direct bare binding is the complete argument. For a
/// nested use, ordinary container validation remains active unless the typed
/// authority produced the diagnostic for that leaf.
pub fn binding_use_authority_owns_module_argument<E, D, B: BindingAddress>(
    file: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    report: &BindingUseReport,
    call: &crate::parser::ModuleCallId,
    argument: &str,
) -> bool {
    file.bare_binding_uses().uses().any(|use_site| {
        matches!(
            &use_site.owner,
            crate::parser::BindingUseOwner::ModuleArgument {
                call: owner,
                argument: owner_argument,
                value_span: _,
            } if owner == call && owner_argument == argument
        ) && binding_use_authority_owns_use(report, use_site)
    })
}

/// Whether binding-use validation owns this sink. A single builtin string
/// construction transfers ownership after admission against the actual sink.
/// Collections retain main's binding and per-element value checks.
pub fn binding_use_authority_owns_use(
    report: &BindingUseReport,
    use_site: &crate::parser::BareBindingUse,
) -> bool {
    !report
        .expression_owned_uses
        .iter()
        .any(|owned| same_authored_use(owned, use_site))
        && (use_site.path.segments().len() == 1
            || report
                .findings()
                .iter()
                .any(|finding| same_authored_use(&finding.use_site, use_site)))
}

/// Whether parsing reserved this entire module argument because a structural
/// leaf could not be represented in the runtime `Value` tree.
pub fn module_argument_has_pending_binding_use<E, D, B: BindingAddress>(
    file: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    call: &crate::parser::ModuleCallId,
    argument: &str,
) -> bool {
    file.bare_binding_uses().pending_slots().any(|slot| {
        matches!(
            &slot.owner,
            crate::parser::BindingUseOwner::ModuleArgument {
                call: owner,
                argument: owner_argument,
                value_span: _,
            } if owner == call && owner_argument == argument
        )
    })
}

trait BindingUseInput<B: BindingAddress> {
    fn value_at_use(
        &self,
        use_site: &crate::parser::BareBindingUse,
    ) -> Option<GenericValue<ParserPlaceholder, B>>;
    fn check_uses(
        &self,
        context: &ValidationTypeContext<'_, B>,
        diagnostic_path: fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
    ) -> BindingUseReport;
}

impl<
    E,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
> BindingUseInput<B> for crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>
{
    fn value_at_use(
        &self,
        use_site: &crate::parser::BareBindingUse,
    ) -> Option<GenericValue<ParserPlaceholder, B>> {
        use crate::parser::{AttributePathSegment, BindingUseOwner};
        let mut segments = use_site.path.segments().iter();
        let AttributePathSegment::Attribute(root) = segments.next()? else {
            return None;
        };
        let mut value = match &use_site.owner {
            BindingUseOwner::Resource(address) => self
                .iter_all_resources()
                .find(|resource| resource.id() == address.resource_id())?
                .attributes()
                .get(root)?
                .clone(),
            BindingUseOwner::ModuleArgument { call, argument, .. } => self
                .module_calls
                .iter()
                .find(|candidate| candidate.id() == call)?
                .argument(argument)?
                .walker_value()
                .clone(),
        };
        for segment in segments {
            value = match (segment, &value) {
                (
                    AttributePathSegment::ListIndex(index),
                    GenericValue::Concrete(crate::resource::ConcreteValue::List(items)),
                ) => items.get(*index)?.clone(),
                (
                    AttributePathSegment::Key(key) | AttributePathSegment::StructField(key),
                    GenericValue::Concrete(crate::resource::ConcreteValue::Map(entries)),
                ) => entries.get(key)?.clone(),
                _ => return None,
            };
        }
        Some(value)
    }

    fn check_uses(
        &self,
        context: &ValidationTypeContext<'_, B>,
        diagnostic_path: fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
    ) -> BindingUseReport {
        check_bare_binding_uses_for_phase(self, context, diagnostic_path)
    }
}

/// Report-only schema-aware authority for every parser-recorded bare binding
/// use. Editor and collect-all validation paths borrow the file through this
/// function; they do not mint production readiness.
fn check_bare_binding_uses_for_phase<E, B, D, F>(
    file: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    context: &ValidationTypeContext<'_, B>,
    diagnostic_path: F,
) -> BindingUseReport
where
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    F: Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
{
    let mut report = BindingUseReport {
        findings: Vec::new(),
        expression_owned_uses: Vec::new(),
    };

    for record in file.bare_binding_uses().records() {
        let evidence = match &record.authored.owner {
            crate::parser::BindingUseOwner::Resource(address) => context
                .expression_report
                .for_resource(address.resource_id()),
            crate::parser::BindingUseOwner::ModuleArgument { .. } => Ok(context.expression_report),
        }
        .and_then(|report| {
            report.expression_in(
                &record.authored.expression_instance,
                record.authored.location(),
            )
        });
        let expected = match &record.authored.owner {
            crate::parser::BindingUseOwner::Resource(address) => {
                resource_sink(file, address, &record.authored.path, context.schemas)
                    .map(RefSink::expression_type)
            }
            crate::parser::BindingUseOwner::ModuleArgument { call, argument, .. } => {
                module_argument_type(file, call, argument, context.module_signatures)
                    .and_then(|expected| {
                        resolve_module_argument_path_type(
                            &expected,
                            &record.authored.path,
                            context.raw_provider_context(),
                        )
                        .ok()
                        .flatten()
                    })
                    .map(|expected| expected.as_type_expr())
            }
        };
        if let Ok(expression) = evidence {
            let fallback = INTERPOLATION_REF_SINK.clone();
            let sink = expected.as_ref().unwrap_or(&fallback);
            let sink_judged = expression.for_sink(sink, context.raw_provider_context());
            if sink_judged.is_interpolation() {
                // Suppress the operand check without claiming the enclosing
                // destination: its resolved String still owes the slot check.
                report.expression_owned_uses.push(record.authored.clone());
                continue;
            }
            if sink_judged.is_single_string_construction()
                && expected.as_ref().is_some_and(|sink| {
                    crate::parser::retained::admit_expression(expression, sink).is_ok()
                })
            {
                report.expression_owned_uses.push(record.authored.clone());
                continue;
            }
        }
        let mut result = match &record.authored.owner {
            crate::parser::BindingUseOwner::Resource(address) => {
                match resource_sink(file, address, &record.authored.path, context.schemas) {
                    Some(sink) => match &record.source {
                        crate::parser::BareBindingSource::Value {
                            value,
                            declared_type,
                        } => resolve_value_source_against_sink(
                            &record.authored,
                            value,
                            declared_type.as_deref(),
                            sink,
                            context,
                            &diagnostic_path,
                        ),
                        crate::parser::BareBindingSource::Structural(handle) => {
                            let existing = BindingResolutionError::Existing(
                                authored_bare_handle_error(record, handle).to_string(),
                            );
                            match enum_target_from_sink(sink) {
                                Some(target) => {
                                    let resolver =
                                        EnumValueResolver::with_defs(target.attr_type, target.defs);
                                    Err(enum_shadow_or_existing(
                                        &record.authored,
                                        &resolver,
                                        existing,
                                    ))
                                }
                                None => Err(existing),
                            }
                        }
                    },
                    None => match &record.source {
                        crate::parser::BareBindingSource::Value { .. } => Ok(()),
                        crate::parser::BareBindingSource::Structural(handle) => {
                            Err(BindingResolutionError::Existing(
                                authored_bare_handle_error(record, handle).to_string(),
                            ))
                        }
                    },
                }
            }
            crate::parser::BindingUseOwner::ModuleArgument {
                call,
                argument,
                value_span: _,
            } => match module_argument_type(file, call, argument, context.module_signatures) {
                Some(expected) => {
                    resolve_record_against_module(record, &expected, context, &diagnostic_path)
                }
                None => match &record.source {
                    crate::parser::BareBindingSource::Value { .. } => Ok(()),
                    crate::parser::BareBindingSource::Structural(handle) => {
                        Err(BindingResolutionError::Existing(
                            authored_bare_handle_error(record, handle).to_string(),
                        ))
                    }
                },
            },
        };

        // A pending slot means the parser deliberately omitted the owning
        // runtime value because this structural leaf cannot be represented in
        // `Value`. Even when its declared nested type is a matching `Ref`, the
        // authored handle therefore needs the normal bare-handle diagnostic;
        // no pending slot can survive as an internal-only fallback.
        if result.is_ok()
            && file
                .bare_binding_uses()
                .has_pending_slot_for(&record.authored)
            && let crate::parser::BareBindingSource::Structural(handle) = &record.source
        {
            result = Err(BindingResolutionError::Existing(
                authored_bare_handle_error(record, handle).to_string(),
            ));
        }

        if let Err(error) = result {
            let finding = BindingUseFinding::error(record.authored.clone(), error);
            if !report
                .findings
                .iter()
                .any(|existing| same_authored_finding(existing, &finding))
            {
                report.findings.push(finding);
            }
        }
    }

    report
}

/// Validate uses from the exact configuration owned by the context.
pub fn check_bare_binding_uses(
    context: &ValidationTypeContext<'_, RelativeBindingName>,
) -> BindingUseReport {
    context.file.check_uses(context, |path| {
        ModuleCallDiagnosticPath::Authored(path.to_owned())
    })
}

pub fn check_expanded_bare_binding_uses(
    context: &ValidationTypeContext<'_, AbsoluteBindingAddress>,
) -> BindingUseReport {
    context.file.check_uses(context, |path| {
        ModuleCallDiagnosticPath::Expanded(path.to_owned())
    })
}

/// Consume the exact file checked by the bare-binding authority.
///
/// Success wraps the exact checked file without exposing a mutable provenance
/// channel, so no later caller can pair an unrelated file with a detached
/// proof. On failure the report contains every authored finding.
pub fn resolve_bare_binding_uses<
    E: crate::parser::IntoExpandedExportParam<AbsoluteBindingAddress>,
>(
    file: crate::parser::ModuleResolvedFile<E>,
    schemas: &SchemaRegistry,
    provider_context: &LoadedProviderContext,
    module_signatures: &crate::module_resolver::ResolvedModuleSignatures,
) -> Result<ResolvedFile<E::Expanded>, ResolveFileError> {
    if !file.expression_report().has_authored_judgement() {
        return Err(crate::parser::retained::UnjudgedConfiguration.into());
    }
    let judged = crate::parser::retained::JudgedFile::new(
        file,
        schemas,
        provider_context.as_provider_context(),
    );
    let report = {
        let context = ValidationTypeContext::new(&judged, provider_context, module_signatures);
        check_expanded_bare_binding_uses(&context)
    };
    let file = judged.into_file();
    finish_binding_resolution(file, report, schemas, provider_context)
}

/// Consume a bootstrap/recovery file using the same incomplete catalog that
/// checked its declarations and module boundaries. Unavailable provider types
/// remain recorded in that catalog's settlement sink.
pub fn resolve_bare_binding_uses_in_session<
    E: crate::parser::IntoExpandedExportParam<AbsoluteBindingAddress>,
>(
    file: crate::parser::ModuleResolvedFile<E>,
    schemas: &SchemaRegistry,
    provider_context: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    module_signatures: &crate::module_resolver::ResolvedModuleSignatures,
) -> Result<ResolvedFile<E::Expanded>, ResolveFileError> {
    if !file.expression_report().has_authored_judgement() {
        return Err(crate::parser::retained::UnjudgedConfiguration.into());
    }
    let judged =
        crate::parser::retained::JudgedFile::new(file, schemas, provider_context.catalog());
    let report = {
        let context =
            ValidationTypeContext::in_session(&judged, provider_context, module_signatures);
        check_expanded_bare_binding_uses(&context)
    };
    let file = judged.into_file();
    finish_binding_resolution(file, report, schemas, provider_context.catalog())
}

fn finish_binding_resolution<E: crate::parser::IntoExpandedExportParam<AbsoluteBindingAddress>>(
    file: crate::parser::ModuleResolvedFile<E>,
    report: BindingUseReport,
    schemas: &SchemaRegistry,
    provider_context: &ProviderContext,
) -> Result<ResolvedFile<E::Expanded>, ResolveFileError> {
    if let Some(finding) = file.expression_report().findings().first() {
        return Err(ResolveFileError::Expression(Box::new(finding.clone())));
    }
    if !report.is_ready() {
        return Err(report.into());
    }

    Ok(ResolvedFile {
        inner: file.into_expanded(schemas, provider_context)?,
    })
}

fn exactly_one<T>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn unique_union_list_element<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let members = crate::schema::union_members_with_defs(attr_type, defs)?;
    exactly_one(members.iter().flatten().filter_map(|member| {
        match member.as_attr().shape_with_defs(defs) {
            Shape::List { element_type, .. } => Some(element_type),
            _ => None,
        }
    }))
}

fn unique_union_map_member<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let members = crate::schema::union_members_with_defs(attr_type, defs)?;
    exactly_one(members.iter().flatten().filter_map(|member| {
        matches!(
            member.as_attr().shape_with_defs(defs),
            Shape::Map { .. } | Shape::Struct { .. }
        )
        .then_some(member.as_attr())
    }))
}

/// Visit every resource reference exactly once while carrying the declared
/// sink type for its structural position.
///
/// Lists, maps, and structs descend in lockstep with their declared type.
/// When a value shape has no corresponding declared position, descendants are
/// still visited with `None` so existence diagnostics are never lost. A Union
/// contributes a nested sink only when exactly one member matches the concrete
/// List or Map value shape. Function arguments likewise have no locally
/// available signature and deliberately receive `None`: the function's result
/// sink says nothing sound about its input positions. Secrets preserve their
/// enclosing sink. Interpolation operands must be scalars that can be
/// rendered to text, independently of the completed string's sink.
fn visit_refs_with_sink_inner<'a, P: PlaceholderKind, B: BindingAddress>(
    value: &GenericValue<P, B>,
    sink: Option<RefSink<'a>>,
    usage: crate::resource::ResourceRefUsage,
    scope: DiagnosticScope,
    f: &mut impl FnMut(
        ReferencePathRef<'_, B>,
        Option<RefSink<'a>>,
        crate::resource::ResourceRefUsage,
        Option<&GenericValue<P, B>>,
        DiagnosticScope,
    ),
) {
    match value {
        GenericValue::Deferred(crate::resource::DeferredValue::ModuleIf(deferred)) => {
            let [condition, then_value, else_value] = deferred.values();
            visit_refs_with_sink_inner(condition, None, usage, scope, f);
            for branch in [then_value, else_value] {
                visit_refs_with_sink_inner(branch, sink, usage, scope, f);
            }
        }
        GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path }) => f(
            ReferencePathRef::<B>::Access(path),
            sink,
            usage,
            None,
            scope,
        ),
        GenericValue::Deferred(crate::resource::DeferredValue::BindingRef { binding }) => f(
            ReferencePathRef::<B>::Binding(binding),
            sink,
            usage,
            None,
            scope,
        ),
        GenericValue::Deferred(crate::resource::DeferredValue::Placeholder(placeholder)) => {
            if let Some(name) = placeholder.module_argument_name()
                && let Some(binding) =
                    crate::resource::module_argument_placeholder::<B>(name.to_string())
            {
                f(
                    ReferencePathRef::<B>::Binding(&binding),
                    sink,
                    usage,
                    None,
                    scope,
                );
            }
        }
        GenericValue::Concrete(crate::resource::ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                visit_refs_with_sink_inner(item, child_sink, usage, scope, f);
            }
        }
        GenericValue::Concrete(crate::resource::ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                let child_sink = sink.and_then(|sink| sink.map_entry(key));
                visit_refs_with_sink_inner(value, child_sink, usage, scope, f);
            }
        }
        GenericValue::Deferred(crate::resource::DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    visit_refs_with_sink_inner(
                        value,
                        Some(RefSink::Interpolation),
                        usage,
                        scope,
                        f,
                    );
                }
            }
        }
        GenericValue::Deferred(
            crate::resource::DeferredValue::FunctionCall { args, .. }
            | crate::resource::DeferredValue::Unavailable { args, .. }
            | crate::resource::DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            // The enclosing sink constrains the function result, not its
            // inputs. Function parameter sites are walked separately with
            // BindingIndex's signature authority.
            for argument in args {
                visit_refs_with_sink_inner(argument.walker_value(), None, usage, scope, f);
            }
        }
        GenericValue::Deferred(crate::resource::DeferredValue::Access(access)) => {
            if let Some(reference) = crate::resource::module_argument_reference_path(value) {
                // Module parameters are typed value placeholders rather than
                // structural references. Project their postfix chain through
                // BindingIndex so `cfg.field` receives the same narrowing and
                // diagnostic origin as an ordinary path.
                f(reference.as_ref(), sink, usage, None, scope);
            } else {
                // Other Access values have computed operands. References
                // nested inside the computation do not inherit its result
                // sink.
                visit_refs_with_sink_inner(access.base(), None, usage, scope, f);
            }
        }
        GenericValue::Deferred(crate::resource::DeferredValue::Secret(inner)) => inner
            .visit_plaintext_reference_inventory(|value, scope| {
                visit_refs_with_sink_inner(value, sink, usage, scope, f)
            }),
        GenericValue::Deferred(crate::resource::DeferredValue::Coalesce { value, default }) => {
            let value_usage = if value.optional_operand().is_some() {
                crate::resource::ResourceRefUsage::CoalesceValue
            } else {
                crate::resource::ResourceRefUsage::Required
            };
            let default_usage = if usage == crate::resource::ResourceRefUsage::CoalesceValue
                && default.optional_operand().is_some()
            {
                crate::resource::ResourceRefUsage::CoalesceValue
            } else {
                crate::resource::ResourceRefUsage::Required
            };
            visit_refs_with_sink_inner(value, sink, value_usage, scope, f);
            visit_refs_with_sink_inner(default, sink, default_usage, scope, f);
        }
        GenericValue::Deferred(crate::resource::DeferredValue::IntegerArithmetic(expression)) => {
            let operand_sink = Some(RefSink::TypeExpr(&TypeExpr::Int));
            visit_refs_with_sink_inner(expression.lhs(), operand_sink, usage, scope, f);
            visit_refs_with_sink_inner(expression.rhs(), operand_sink, usage, scope, f);
        }
        GenericValue::Concrete(crate::resource::ConcreteValue::String(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::EnumIdentifier(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::CanonicalEnum(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::Int(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::Float(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::Bool(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::Duration(_))
        | GenericValue::Concrete(crate::resource::ConcreteValue::StringList(_))
        | GenericValue::Deferred(crate::resource::DeferredValue::Unknown(_)) => {}
    }
}

/// Visit values whose postfix path needs inference in addition to the normal
/// reference walk. Computed Access values need their result checked; a
/// canonical ResourceRef with path segments also needs inference so a known
/// scalar cannot be indexed merely because BindingIndex reserves shape
/// diagnostics for upstream references.
fn collect_ref_diagnostics_with_sink<'a, P: PlaceholderKind, B: BindingAddress, E: InsideSecret>(
    value: &GenericValue<P, B>,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(
        ReferencePathRef<'_, B>,
        Option<RefSink<'a>>,
        crate::resource::ResourceRefUsage,
        Option<&GenericValue<P, B>>,
        &mut Vec<E>,
    ),
) -> Vec<E> {
    let mut findings = Vec::new();
    visit_refs_with_sink_inner(
        value,
        sink,
        crate::resource::ResourceRefUsage::Required,
        DiagnosticScope::Public,
        &mut |path, sink, usage, candidate, scope| {
            let mut local = Vec::new();
            f(path, sink, usage, candidate, &mut local);
            findings.extend(local.into_iter().map(|finding| {
                if scope.is_secret() {
                    finding.inside_secret()
                } else {
                    finding
                }
            }));
        },
    );
    findings
}

type ReferenceDiagnosticMessage = SecretDiagnosticError<String>;

/// A mismatch and its reference origin, captured before admission reduces the
/// value to a type error. CLI and editor presentation share this typed source.
#[derive(Debug, Clone)]
pub struct ReferenceMismatch {
    types: MismatchTypeNames,
    origin: Option<String>,
    assignment_origin: Option<String>,
    attribute: String,
    path: crate::schema::LeafPath,
}

impl ReferenceMismatch {
    pub fn for_attribute<P: PlaceholderKind, B: BindingAddress>(
        value: &GenericValue<P, B>,
        path: &crate::schema::LeafPath,
        attribute: &str,
        types: &MismatchTypeNames,
    ) -> Self {
        let leaf = path
            .segments()
            .iter()
            .try_fold(value, |value, step| match (value, step) {
                (
                    GenericValue::Concrete(crate::resource::ConcreteValue::List(values)),
                    crate::schema::LeafPathSegment::ListIndex(index),
                ) => values.get(*index),
                (
                    GenericValue::Concrete(crate::resource::ConcreteValue::Map(values)),
                    crate::schema::LeafPathSegment::StructField(name),
                ) => values.get(name.as_str()),
                (
                    GenericValue::Concrete(crate::resource::ConcreteValue::Map(values)),
                    crate::schema::LeafPathSegment::MapKey(key),
                ) => values.get(key.expose_plaintext()),
                _ => None,
            });
        let origin = match leaf {
            Some(GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path })) => {
                Some(format!("from {}", path.to_dot_string()))
            }
            Some(GenericValue::Deferred(crate::resource::DeferredValue::BindingRef {
                binding,
            })) => Some(format!("from {binding}")),
            _ => None,
        };
        // Human assignments retain main's historical recursive presentation.
        // Diagnostic clients use only the direct leaf captured above.
        let mut references = Vec::new();
        if let Some(leaf) = leaf {
            leaf.visit_resource_refs_for_diagnostics(&mut |reference, _| {
                references.push(reference.diagnostic_path().to_dot_string())
            });
            leaf.visit_binding_refs_for_diagnostics(&mut |binding| {
                references.push(binding.diagnostic_path().to_dot_string())
            });
        }
        Self {
            types: types.clone(),
            attribute: attribute.to_owned(),
            path: path.clone(),
            origin,
            assignment_origin: references
                .into_iter()
                .next()
                .map(|path| format!("from {path}")),
        }
    }
    pub fn message(&self) -> String {
        self.render(
            (!self.path.segments().is_empty()).then_some(self.attribute.as_str()),
            self.origin.as_ref(),
        )
    }

    pub fn assignment_message(&self) -> String {
        self.render(Some(&self.attribute), self.assignment_origin.as_ref())
    }

    fn render(&self, attribute: Option<&str>, origin: Option<&String>) -> String {
        reference_type_mismatch_message(
            attribute,
            &self.types,
            origin.map(|origin| origin as &dyn std::fmt::Display),
        )
    }
}

/// Render a reference mismatch for an attribute or a top-level value.
/// `origin` includes its label (for example, `from producer.output`).
pub fn reference_type_mismatch_message(
    attribute: Option<&str>,
    types: &MismatchTypeNames,
    origin: Option<&dyn std::fmt::Display>,
) -> String {
    let message = match attribute {
        Some(attribute) => format!("cannot assign {} to '{attribute}': {types}", types.actual()),
        None => format!("Type mismatch: {types}"),
    };
    match origin {
        Some(origin) => format!("{message} ({origin})"),
        None => message,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferenceFindingLocation {
    Resource { location: String, attribute: String },
    Attribute(String),
    Export(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferenceFindingKind {
    UnknownAttribute(SecretDiagnosticError<RefTypeError>),
    UnknownBinding(ReferenceDiagnosticMessage),
    InvalidValue(ReferenceDiagnosticMessage),
    SchemaUnavailable(SecretDiagnosticError<Box<crate::parser::CustomTypeValidationError>>),
    TypeMismatch {
        types: MismatchTypeNames,
        path: ReferenceDiagnosticMessage,
        origin: Option<ReferenceDiagnosticMessage>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceFinding {
    location: ReferenceFindingLocation,
    kind: ReferenceFindingKind,
}

impl ReferenceFinding {
    /// Existence diagnostics own these errors in renderers that also consume
    /// the attribute admission report.
    pub fn is_lookup_error(&self) -> bool {
        match &self.kind {
            ReferenceFindingKind::UnknownAttribute(_) | ReferenceFindingKind::UnknownBinding(_) => {
                true
            }
            ReferenceFindingKind::InvalidValue(_)
            | ReferenceFindingKind::SchemaUnavailable(_)
            | ReferenceFindingKind::TypeMismatch { .. } => false,
        }
    }

    fn new(location: &ReferenceFindingLocation, kind: ReferenceFindingKind) -> Self {
        Self {
            location: location.clone(),
            kind,
        }
    }
}

impl InsideSecret for ReferenceFinding {
    fn inside_secret(self) -> Self {
        let kind = match self.kind {
            ReferenceFindingKind::UnknownAttribute(message) => {
                ReferenceFindingKind::UnknownAttribute(message.inside_secret())
            }
            ReferenceFindingKind::UnknownBinding(message) => {
                ReferenceFindingKind::UnknownBinding(message.inside_secret())
            }
            ReferenceFindingKind::InvalidValue(message) => {
                ReferenceFindingKind::InvalidValue(message.inside_secret())
            }
            ReferenceFindingKind::SchemaUnavailable(error) => {
                ReferenceFindingKind::SchemaUnavailable(error.inside_secret())
            }
            ReferenceFindingKind::TypeMismatch {
                types,
                path,
                origin,
            } => ReferenceFindingKind::TypeMismatch {
                types,
                path: path.inside_secret(),
                origin: origin.map(InsideSecret::inside_secret),
            },
        };
        Self { kind, ..self }
    }
}

impl std::fmt::Display for ReferenceFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ReferenceFindingKind::*;
        match &self.location {
            ReferenceFindingLocation::Resource {
                location,
                attribute,
            } => {
                write!(f, "{location}: ")?;
                if matches!(
                    self.kind,
                    UnknownAttribute(SecretDiagnosticError::InsideSecret)
                        | UnknownBinding(SecretDiagnosticError::InsideSecret)
                        | InvalidValue(SecretDiagnosticError::InsideSecret)
                        | SchemaUnavailable(SecretDiagnosticError::InsideSecret)
                ) {
                    write!(f, "attribute '{attribute}': ")?;
                }
            }
            ReferenceFindingLocation::Attribute(name) => write!(f, "attribute '{name}': ")?,
            ReferenceFindingLocation::Export(name) => write!(f, "export '{name}': ")?,
        }
        match &self.kind {
            UnknownAttribute(message @ SecretDiagnosticError::InsideSecret) => {
                write!(f, "unknown attribute: {message}")
            }
            UnknownBinding(message @ SecretDiagnosticError::InsideSecret) => {
                write!(f, "unknown binding: {message}")
            }
            UnknownAttribute(error) => error.fmt(f),
            UnknownBinding(message) | InvalidValue(message) => message.fmt(f),
            SchemaUnavailable(error) => error.fmt(f),
            TypeMismatch {
                types,
                path,
                origin,
            } => {
                match &self.location {
                    ReferenceFindingLocation::Resource { attribute, .. } => {
                        return f.write_str(&reference_type_mismatch_message(
                            Some(attribute),
                            types,
                            origin
                                .as_ref()
                                .map(|origin| origin as &dyn std::fmt::Display),
                        ));
                    }
                    ReferenceFindingLocation::Attribute(_) => write!(f, "type mismatch: {types}")?,
                    ReferenceFindingLocation::Export(_) => {
                        write!(f, "type mismatch for '{path}': {types}")?
                    }
                }
                if let Some(origin) = origin {
                    write!(f, " ({origin})")?;
                }
                Ok(())
            }
        }
    }
}

fn visit_computed_accesses_with_sink<'a, B: BindingAddress, E: InsideSecret>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(&GenericValue<ParserPlaceholder, B>, Option<RefSink<'a>>) -> Vec<E>,
) -> Vec<E> {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart};
    let mut findings = Vec::new();
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let [condition, then_value, else_value] = deferred.values();
            findings.extend(visit_computed_accesses_with_sink(condition, None, f));
            for branch in [then_value, else_value] {
                findings.extend(visit_computed_accesses_with_sink(branch, sink, f));
            }
        }
        GenericValue::Deferred(DeferredValue::Access(access)) => {
            if crate::resource::module_argument_reference_path(value).is_none() {
                findings.extend(f(value, sink));
                findings.extend(visit_computed_accesses_with_sink(access.base(), None, f));
            }
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { path })
            if !path.segments().is_empty() =>
        {
            findings.extend(f(value, sink))
        }
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                findings.extend(visit_computed_accesses_with_sink(item, child_sink, f));
            }
        }
        GenericValue::Concrete(ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                findings.extend(visit_computed_accesses_with_sink(
                    value,
                    sink.and_then(|sink| sink.map_entry(key)),
                    f,
                ));
            }
        }
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let InterpolationPart::Expr(value) = part {
                    findings.extend(visit_computed_accesses_with_sink(
                        value,
                        Some(RefSink::Interpolation),
                        f,
                    ));
                }
            }
        }
        GenericValue::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for argument in args {
                findings.extend(visit_computed_accesses_with_sink(
                    argument.walker_value(),
                    None,
                    f,
                ));
            }
        }
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            findings.extend(
                inner
                    .try_inspect(|value| {
                        Err::<(), _>(visit_computed_accesses_with_sink(value, sink, f))
                    })
                    .err()
                    .into_iter()
                    .flatten(),
            );
        }
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            findings.extend(visit_computed_accesses_with_sink(value, sink, f));
            findings.extend(visit_computed_accesses_with_sink(default, sink, f));
        }
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let result_sink = match sink {
                Some(RefSink::Interpolation) => Some(RefSink::TypeExpr(&INT_REF_SINK)),
                other => other,
            };
            findings.extend(f(value, result_sink));
            let operand_sink = Some(RefSink::TypeExpr(&INT_REF_SINK));
            findings.extend(visit_computed_accesses_with_sink(
                expression.lhs(),
                operand_sink,
                f,
            ));
            findings.extend(visit_computed_accesses_with_sink(
                expression.rhs(),
                operand_sink,
                f,
            ));
        }
        GenericValue::Concrete(_)
        | GenericValue::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => {}
    }
    findings
}

/// A statically-known postfix access error shared by CLI validation and LSP
/// diagnostics. Keeping the inference/error selection here prevents editor
/// callers from silently treating a new inference error variant as success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredAccessTypeError {
    InvalidAccess {
        error: inference::InferenceError,
    },
    TypeMismatch {
        types: MismatchTypeNames,
        origin: String,
    },
}

impl std::fmt::Display for DeferredAccessTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAccess { error } => error.fmt(f),
            Self::TypeMismatch { types, origin } => write!(f, "Type mismatch: {types} ({origin})"),
        }
    }
}

impl InsideSecret for DeferredAccessTypeError {
    fn inside_secret(self) -> Self {
        match self {
            Self::InvalidAccess { error } => Self::InvalidAccess {
                error: error.inside_secret(),
            },
            Self::TypeMismatch { types, .. } => Self::TypeMismatch {
                types,
                origin: format!("from {}", crate::schema::SECRET_VALUE_MASK),
            },
        }
    }
}

impl DeferredAccessTypeError {
    pub(crate) fn reference_mismatch<B: BindingAddress>(
        mismatch: crate::binding_index::RefTypeMismatch,
        reference: ReferencePathRef<'_, B>,
    ) -> Self {
        Self::TypeMismatch {
            types: mismatch.type_names().clone(),
            origin: mismatch.origin_description(reference),
        }
    }

    /// Return the authored operator span when this computed-expression error
    /// came from integer arithmetic. Frontends use this only for source
    /// anchoring; the diagnostic text and validity decision remain in core.
    fn integer_operator_span(&self) -> Option<crate::integer_arithmetic::IntegerOperatorSpan> {
        match self {
            Self::InvalidAccess {
                error:
                    inference::InferenceError::InvalidIntegerArithmeticOperand { operator_span, .. },
            } => Some(operator_span.clone()),
            Self::InvalidAccess { .. } | Self::TypeMismatch { .. } => None,
        }
    }
}

/// One arithmetic-specific failure found while walking a computed value.
/// The shared checker returns this detail inside `Located`; coordinate-only
/// arithmetic checks are not an external diagnostic API.
///
/// ```compile_fail,E0603
/// use carina_core::validation::check_integer_arithmetic_types;
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::validation::IntegerArithmeticTypeFinding;
/// fn fabricate(finding: IntegerArithmeticTypeFinding) { let span = finding.operator_span(); }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerArithmeticTypeFinding {
    error: DeferredAccessTypeError,
}

impl IntegerArithmeticTypeFinding {
    pub fn error(&self) -> &DeferredAccessTypeError {
        &self.error
    }
}

impl InsideSecret for crate::parser::Located<IntegerArithmeticTypeFinding> {
    fn inside_secret(self) -> Self {
        let (location, finding) = self.into_parts();
        crate::parser::Located::new(
            location,
            IntegerArithmeticTypeFinding {
                error: finding.error.inside_secret(),
            },
        )
    }
}

impl std::fmt::Display for IntegerArithmeticTypeFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// Check a deferred postfix result against its sink with the canonical
/// inference authority. Unknown bases remain unchecked; known-invalid access
/// and known result mismatches are errors.
pub fn check_deferred_access_type<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    expected: crate::schema::AssignmentSink<'_, crate::schema::Compiled>,
    expression: &crate::parser::retained::JudgedExpr,
    _schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    let value = expression.assignment_value(value);
    // Canonical reference-rooted postfix expressions are ResourceRefs. Route
    // those through BindingIndex so managed resources and wait aliases share
    // the same schema/value-source authority. This decision belongs here,
    // rather than in every CLI/LSP caller, so a new binding kind cannot be
    // silently skipped by a copied `InferenceBinding` predicate.
    if let GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path }) = value {
        return match binding_index.postfix_ref_type(path) {
            RefType::Typed(source) => match source.is_assignable_to_sink(expected, binding_index) {
                Ok(()) => Ok(()),
                Err(RefTypeAssignabilityError::Unchecked(reason)) => {
                    Err(unchecked_assignment_error(reason))
                }
                Err(RefTypeAssignabilityError::Mismatch(mismatch)) => Err(
                    DeferredAccessTypeError::reference_mismatch(mismatch, path.into()),
                ),
            },
            RefType::UnknownAttribute(RefTypeError::InvalidAccess { step, actual, .. }) => {
                Err(DeferredAccessTypeError::InvalidAccess {
                    error: inference::InferenceError::InvalidAccess {
                        base_type: actual,
                        step,
                    },
                })
            }
            // Existence/field errors retain their dedicated reference
            // diagnostics; unknown type surfaces remain deliberately
            // unchecked, as before.
            RefType::Unchecked
            | RefType::UnknownBinding { .. }
            | RefType::UnknownAttribute(
                RefTypeError::UnknownAttribute { .. }
                | RefTypeError::UnknownStructField { .. }
                | RefTypeError::UnknownUpstreamExport { .. },
            ) => Ok(()),
        };
    }

    check_inferred_assignment(
        value,
        expected,
        expression.static_type().cloned(),
        binding_index,
    )
}

/// Check values produced by state/runtime evaluation, which have no authored
/// expression. Parser-phase projections cannot enter this API.
pub fn check_runtime_value_type<B: BindingAddress>(
    input: inference::RuntimeTypeInput<'_, B>,
    expected: crate::schema::AssignmentSink<'_, crate::schema::Compiled>,
    bindings: &inference::InferenceBindings<crate::resource::NoPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    check_inferred_assignment(
        input.value(),
        expected,
        inference::infer_runtime_type(input, bindings, schemas, binding_index.provider_context()),
        binding_index,
    )
}

fn check_inferred_assignment<P: PlaceholderKind, B: BindingAddress>(
    value: &GenericValue<P, B>,
    expected: crate::schema::AssignmentSink<'_, crate::schema::Compiled>,
    inferred: Result<TypeExpr, crate::static_type::InferenceIssue>,
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    let actual = match inferred {
        Ok(actual) => actual,
        Err(crate::static_type::InferenceIssue::Failed(
            error @ (inference::InferenceError::InvalidAccess { .. }
            | inference::InferenceError::MissingEvidence(_)),
        ))
        | Err(crate::static_type::InferenceIssue::Failed(
            error @ inference::InferenceError::UnknownAttribute { .. },
        )) => {
            return Err(DeferredAccessTypeError::InvalidAccess { error });
        }
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::IncompatibleCoalesceTypes { types },
        )) => {
            return Err(DeferredAccessTypeError::TypeMismatch {
                types,
                origin: "from coalesce default".to_string(),
            });
        }
        Err(crate::static_type::InferenceIssue::Failed(
            error @ (inference::InferenceError::DirectoryConstraint(_)
            | inference::InferenceError::InsideSecret { .. }
            | inference::InferenceError::SecretAssignmentMismatch { .. }
            | inference::InferenceError::InvalidIntegerArithmeticOperand { .. }
            | inference::InferenceError::HeterogeneousCollection { .. }
            | inference::InferenceError::UnknownType { .. }),
        )) => return Err(DeferredAccessTypeError::InvalidAccess { error }),
        // Dynamic result types and unavailable authorities cannot be checked
        // soundly at this phase. Spell every inference variant out so adding a
        // new failure reason forces an explicit checked/unchecked decision.
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::UnknownBinding { .. }
            | inference::InferenceError::CyclicVirtualBinding { .. },
        ))
        | Err(crate::static_type::InferenceIssue::NotInferable(
            crate::static_type::NotInferableReason::MissingValue
            | crate::static_type::NotInferableReason::ValueDependent { .. }
            | crate::static_type::NotInferableReason::PlaceholderTypeUnavailable { .. }
            | crate::static_type::NotInferableReason::NonInferableBinding { .. }
            | crate::static_type::NotInferableReason::SchemaUnavailable { .. }
            | crate::static_type::NotInferableReason::CustomTypeSchemaUnavailable { .. },
        )) => return Ok(()),
    };
    let actual = binding_index
        .resolve_declared_type(&actual)
        .map_err(|error| DeferredAccessTypeError::InvalidAccess {
            error: inference::InferenceError::UnknownType {
                reason: error.to_string(),
            },
        })?;
    let Some(actual) = lift_type_expr(&actual) else {
        return Ok(());
    };
    let actual = crate::schema::TypeInSchema::schemaless(&actual);
    if binding_index
        .check_type_assignment(actual, expected)
        .map_err(unchecked_assignment_error)?
    {
        return Ok(());
    }
    Err(DeferredAccessTypeError::TypeMismatch {
        types: MismatchTypeNames::for_types(*expected, actual),
        origin: format!("from {}", crate::value::format_value(value)),
    })
}

fn unchecked_assignment_error(reason: impl std::fmt::Display) -> DeferredAccessTypeError {
    DeferredAccessTypeError::InvalidAccess {
        error: inference::InferenceError::UnknownType {
            reason: reason.to_string(),
        },
    }
}

fn invalid_integer_operand<B: BindingAddress>(
    expression: &crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
    side: crate::integer_arithmetic::IntegerOperandSide,
    actual: String,
) -> DeferredAccessTypeError {
    DeferredAccessTypeError::InvalidAccess {
        error: inference::InferenceError::InvalidIntegerArithmeticOperand {
            op: expression.op(),
            side,
            actual,
            operator_span: expression.operator_span(),
        },
    }
}

trait IntegerOperandEvidence<B: BindingAddress> {
    fn operand<'a>(
        &'a self,
        expression: &crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
        side: crate::integer_arithmetic::IntegerOperandSide,
    ) -> Result<&'a crate::parser::retained::JudgedExpr, crate::parser::retained::MissingEvidence>;
}
impl<B: BindingAddress> IntegerOperandEvidence<B> for crate::parser::retained::ExpressionReport<B> {
    fn operand<'a>(
        &'a self,
        expression: &crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
        side: crate::integer_arithmetic::IntegerOperandSide,
    ) -> Result<&'a crate::parser::retained::JudgedExpr, crate::parser::retained::MissingEvidence>
    {
        let span = expression.operator_span();
        let location = span.location().ok_or_else(|| {
            crate::parser::retained::MissingEvidence::new("integer operator location")
        })?;
        self.operand(location, side)
    }
}
impl<B: BindingAddress> IntegerOperandEvidence<B>
    for crate::parser::retained::JudgedNodeRef<'_, B>
{
    fn operand<'a>(
        &'a self,
        expression: &crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
        side: crate::integer_arithmetic::IntegerOperandSide,
    ) -> Result<&'a crate::parser::retained::JudgedExpr, crate::parser::retained::MissingEvidence>
    {
        let id = expression.operator_span().node_id().ok_or_else(|| {
            crate::parser::retained::MissingEvidence::new("integer operator parser identity")
        })?;
        let node = self.retained_node(id)?;
        let index = match side {
            crate::integer_arithmetic::IntegerOperandSide::Left => 0,
            crate::integer_arithmetic::IntegerOperandSide::Right => 1,
        };
        node.children()
            .nth(index)
            .ok_or_else(|| crate::parser::retained::MissingEvidence::new("integer operand child"))?
            .map(|node| node.judgement())
    }
}

fn prove_integer_operand<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    expression: &crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
    side: crate::integer_arithmetic::IntegerOperandSide,
    report: &(impl IntegerOperandEvidence<B> + ?Sized),
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    let integer = lift_type_expr(&TypeExpr::Int).expect("built-in Int lifts");
    let integer_sink = integer.assignment_sink(&binding_index.provider_context().custom_types);
    let reference_type = match value {
        GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path }) => {
            Some(binding_index.postfix_ref_type(path))
        }
        GenericValue::Deferred(crate::resource::DeferredValue::BindingRef { binding }) => {
            Some(binding_index.ref_type(ReferencePathRef::Binding(binding)))
        }
        GenericValue::Deferred(crate::resource::DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::UpstreamExport { path },
        )) => Some(binding_index.postfix_ref_type(path)),
        GenericValue::Deferred(crate::resource::DeferredValue::Secret(inner)) => {
            return {
                inner.try_inspect(|secret_value| {
                    prove_integer_operand(secret_value, expression, side, report, binding_index)
                })
            };
        }
        GenericValue::Deferred(crate::resource::DeferredValue::Coalesce { value, default }) => {
            prove_integer_operand(value, expression, side, report, binding_index)?;
            return prove_integer_operand(default, expression, side, report, binding_index);
        }
        GenericValue::Deferred(crate::resource::DeferredValue::ModuleIf(deferred)) => {
            prove_integer_operand(
                deferred.then_value(),
                expression,
                side,
                report,
                binding_index,
            )?;
            return prove_integer_operand(
                deferred.else_value(),
                expression,
                side,
                report,
                binding_index,
            );
        }
        // The nested node has an intrinsic Int result; its own callback in the
        // same walk proves its operands and owns any inner diagnostic span.
        GenericValue::Deferred(crate::resource::DeferredValue::IntegerArithmetic(_)) => {
            return Ok(());
        }
        GenericValue::Deferred(crate::resource::DeferredValue::Placeholder(placeholder)) => {
            placeholder
                .module_argument_name()
                .and_then(|name| {
                    crate::resource::module_argument_placeholder::<B>(name.to_string())
                })
                .map(|binding| binding_index.ref_type(ReferencePathRef::Binding(&binding)))
        }
        GenericValue::Concrete(_)
        | GenericValue::Deferred(
            crate::resource::DeferredValue::Interpolation(_)
            | crate::resource::DeferredValue::FunctionCall { .. }
            | crate::resource::DeferredValue::Unavailable { .. }
            | crate::resource::DeferredValue::ResolvedFunctionCall { .. }
            | crate::resource::DeferredValue::Access(_)
            | crate::resource::DeferredValue::Unknown(_),
        ) => None,
    };

    if let Some(RefType::Typed(source)) = reference_type {
        return source
            .is_assignable_to_sink(integer_sink, binding_index)
            .map_err(|error| match error {
                RefTypeAssignabilityError::Mismatch(mismatch) => invalid_integer_operand(
                    expression,
                    side,
                    mismatch.actual_type_name().to_string(),
                ),
                RefTypeAssignabilityError::Unchecked(reason) => unchecked_assignment_error(reason),
            });
    }

    match report
        .operand(expression, side)
        .map_err(|error| DeferredAccessTypeError::InvalidAccess {
            error: error.into(),
        })?
        .static_type()
        .cloned()
    {
        Ok(TypeExpr::Int) => Ok(()),
        Ok(TypeExpr::UnloadedSchemaType(_))
        | Err(crate::static_type::InferenceIssue::NotInferable(
            crate::static_type::NotInferableReason::CustomTypeSchemaUnavailable { .. },
        )) => Ok(()),
        Ok(actual) => Err(invalid_integer_operand(
            expression,
            side,
            MismatchTypeNames::for_type_exprs(&TypeExpr::Int, &actual)
                .actual()
                .to_string(),
        )),
        Err(crate::static_type::InferenceIssue::Failed(
            error @ inference::InferenceError::InvalidIntegerArithmeticOperand { .. },
        )) => Err(DeferredAccessTypeError::InvalidAccess { error }),
        Err(error) => Err(invalid_integer_operand(
            expression,
            side,
            format!("unproven type ({error})"),
        )),
    }
}

fn check_integer_result_sink<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'_>>,
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    let integer = AttributeType::int();
    let actual = crate::schema::TypeInSchema::schemaless(&integer);
    let expected = match sink {
        Some(RefSink::EffectiveType {
            attr_type,
            defs,
            ancestry: _,
            owner,
        }) => attr_type.in_schema(defs).with_authority(
            &binding_index.provider_context().custom_types,
            crate::schema::SinkAuthority::Owner(owner),
        ),
        Some(RefSink::TypeExpr(type_expr)) => {
            let resolved = binding_index
                .resolve_declared_type(type_expr)
                .map_err(|error| DeferredAccessTypeError::InvalidAccess {
                    error: inference::InferenceError::UnknownType {
                        reason: error.to_string(),
                    },
                })?;
            // Nominal evidence can be lifted for identity comparisons, but it
            // cannot establish the scalar kind required by an integer result.
            // Keep this sink unchecked until its declaration is available.
            if matches!(resolved, TypeExpr::UnloadedSchemaType(_)) {
                return Ok(());
            }
            let Some(expected) = lift_type_expr(&resolved) else {
                return Ok(());
            };
            if binding_index
                .check_type_assignment(
                    actual,
                    expected.assignment_sink(&binding_index.provider_context().custom_types),
                )
                .map_err(unchecked_assignment_error)?
            {
                return Ok(());
            }
            return Err(DeferredAccessTypeError::TypeMismatch {
                types: MismatchTypeNames::for_types(
                    crate::schema::TypeInSchema::schemaless(&expected),
                    actual,
                ),
                origin: format!("from {}", crate::value::format_value(value)),
            });
        }
        // Interpolation is the sole textual boundary. The arithmetic node
        // itself still has an Int result, and no external sink means its own
        // intrinsic Int contract is sufficient.
        Some(RefSink::Interpolation) | None => return Ok(()),
    };
    if binding_index
        .check_type_assignment(actual, expected)
        .map_err(unchecked_assignment_error)?
    {
        Ok(())
    } else {
        Err(DeferredAccessTypeError::TypeMismatch {
            types: MismatchTypeNames::for_types(*expected, actual),
            origin: format!("from {}", crate::value::format_value(value)),
        })
    }
}

fn check_integer_arithmetic_types_with_sink<'a, B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'a>>,
    report: &(impl IntegerOperandEvidence<B> + ?Sized),
    binding_index: &BindingIndex<'_, B>,
) -> Vec<crate::parser::Located<IntegerArithmeticTypeFinding>> {
    let findings =
        visit_computed_accesses_with_sink(value, sink, &mut |candidate, candidate_sink| {
            let GenericValue::Deferred(crate::resource::DeferredValue::IntegerArithmetic(
                expression,
            )) = candidate
            else {
                return Vec::new();
            };
            let result = prove_integer_operand(
                expression.lhs(),
                expression,
                crate::integer_arithmetic::IntegerOperandSide::Left,
                report,
                binding_index,
            )
            .and_then(|()| {
                prove_integer_operand(
                    expression.rhs(),
                    expression,
                    crate::integer_arithmetic::IntegerOperandSide::Right,
                    report,
                    binding_index,
                )
            })
            .and_then(|()| check_integer_result_sink(candidate, candidate_sink, binding_index));
            let Err(error) = result else {
                return Vec::new();
            };
            let operator_span = error
                .integer_operator_span()
                .unwrap_or_else(|| expression.operator_span());
            vec![crate::parser::Located::new(
                operator_span
                    .location()
                    .expect("integer operator validated at the analysis boundary")
                    .clone(),
                IntegerArithmeticTypeFinding { error },
            )]
        });
    let mut unique = Vec::new();
    for finding in findings {
        if !unique.iter().any(
            |previous: &crate::parser::Located<IntegerArithmeticTypeFinding>| {
                previous.location() == finding.location()
                    && previous.detail().to_string() == finding.detail().to_string()
            },
        ) {
            unique.push(finding);
        }
    }
    unique
}

impl<D, B: BindingAddress, E>
    crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B, E>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub fn integer_arithmetic_findings(
        &self,
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        name: &str,
        schema: crate::schema::OwnerBoundResourceSchema<'_>,
        binding_index: &crate::binding_index::BindingIndex<'_, B>,
    ) -> Vec<crate::parser::Located<crate::validation::IntegerArithmeticTypeFinding>> {
        let report = match self.file().expression_report().for_resource(resource.id()) {
            Ok(report) => report,
            Err(error) => {
                return vec![crate::parser::Located::new(
                    resource
                        .attribute_value_location(name, &crate::schema::FieldPath::new())
                        .clone(),
                    IntegerArithmeticTypeFinding {
                        error: DeferredAccessTypeError::InvalidAccess {
                            error: error.into(),
                        },
                    },
                )];
            }
        };
        let attributes = resource.attributes();
        let sink = schema
            .effective_attribute_by_authored_name(name)
            .expect("a checked attribute has an effective sink")
            .value_type();
        check_integer_arithmetic_types(
            &attributes[name],
            sink,
            schema.schema_type_owner(),
            report,
            binding_index,
        )
    }
}

/// Validate every integer-arithmetic node in a value tree against both its
/// authored resource sink and its invariant Int operand sinks. This is the
/// shared source for CLI and LSP arithmetic diagnostics; callers add only
/// their surrounding resource context and source-range conversion.
fn check_integer_arithmetic_types<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: crate::schema::EffectiveType<'_>,
    owner: &crate::parser::SchemaTypeOwner,
    report: &crate::parser::retained::ExpressionReport<B>,
    binding_index: &BindingIndex<'_, B>,
) -> Vec<crate::parser::Located<IntegerArithmeticTypeFinding>> {
    check_integer_arithmetic_types_with_sink(
        value,
        Some(RefSink::from_effective_type(sink, owner)),
        report,
        binding_index,
    )
}

fn append_unique_integer_arithmetic_findings<B: BindingAddress>(
    findings: &mut Vec<crate::parser::Located<IntegerArithmeticTypeFinding>>,
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'_>>,
    report: &crate::parser::retained::ExpressionReport<B>,
    binding_index: &BindingIndex<'_, B>,
) {
    for finding in check_integer_arithmetic_types_with_sink(value, sink, report, binding_index) {
        let duplicate = findings.iter().any(|existing| {
            existing.to_string() == finding.to_string() && existing.location() == finding.location()
        });
        if !duplicate {
            findings.push(finding);
        }
    }
}

/// Validate arithmetic in every non-resource value declaration in a parsed
/// file. Resource attributes retain their external schema sink and are checked
/// separately by [`crate::parser::AnalysisTree::integer_arithmetic_findings`]; this pass covers values
/// that may otherwise be unused, including module attributes, provider/backend
/// values, deferred iterables, and user-function bodies.
/// A raw file cannot bypass the boundary that requires operator provenance.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, validation::{check_file_integer_arithmetic_values, inference::InferenceBindings}, schema::SchemaRegistry, binding_index::BindingIndex};
/// fn analyze(file: &ParsedFile, bindings: &BindingIndex<'_>) {
///     check_file_integer_arithmetic_values(file, bindings);
/// }
/// ```
pub fn check_file_integer_arithmetic_values<
    B: BindingAddress,
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    binding_index: &BindingIndex<'_, B>,
) -> Vec<crate::parser::Located<IntegerArithmeticTypeFinding>> {
    let parsed = judged.file();
    let report = judged.report();
    let mut findings = Vec::new();
    let mut check = |value: &GenericValue<ParserPlaceholder, B>, sink: Option<RefSink<'_>>| {
        append_unique_integer_arithmetic_findings(
            &mut findings,
            value,
            sink,
            report,
            binding_index,
        );
    };

    for value in parsed.variables.values() {
        check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)));
    }
    for call in &parsed.module_calls {
        for (_, argument) in call.arguments() {
            check(
                argument.walker_value(),
                Some(RefSink::TypeExpr(&INT_REF_SINK)),
            );
        }
    }
    for argument in &parsed.arguments {
        if let Some(value) = &argument.default {
            check(value, Some(RefSink::TypeExpr(&argument.static_type)));
        }
    }
    for attribute in &parsed.attribute_params {
        if let Some(expression) = &attribute.value {
            match expression {
                crate::parser::BoundaryExpression::Value(value) => {
                    check(value, attribute.static_type.as_ref().map(RefSink::TypeExpr))
                }
                crate::parser::BoundaryExpression::Handle(_) => {}
            }
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value() {
            check(value, export.type_expr_opt().map(RefSink::TypeExpr));
        }
    }
    for provider in &parsed.providers {
        for value in provider
            .attributes
            .values()
            .chain(provider.default_tags.values())
            .chain(provider.unresolved_attributes.values())
        {
            check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)));
        }
    }
    if let Some(backend) = &parsed.backend {
        for value in backend.attributes.values() {
            check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)));
        }
    }
    for state_block in &parsed.state_blocks {
        if let crate::parser::StateBlock::Import { id, .. } = state_block {
            check(id, Some(RefSink::TypeExpr(&INT_REF_SINK)));
        }
    }
    // Wait arithmetic is fully evaluated while parsing; the typed RHS is a
    // placeholder-free literal and therefore has no deferred arithmetic node
    // for this parser-stage proof walk.
    for deferred in parsed.deferred_for_sites() {
        check(deferred.iterable, Some(RefSink::TypeExpr(&INT_REF_SINK)));
    }

    for function in parsed.user_functions.values() {
        for parameter in &function.params {
            if let Some(value) = &parameter.default {
                check(value, parameter.param_type.as_ref().map(RefSink::TypeExpr));
            }
        }
        for (_, value, _) in &function.local_lets {
            check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)));
        }
        check(
            &function.body.value,
            function.return_type.as_ref().map(RefSink::TypeExpr),
        );
    }

    findings
}

struct ResourceReferenceContext<'a, 'schema, B: BindingAddress> {
    location: ReferenceFindingLocation,
    bindings: &'a BindingIndex<'schema, B>,
    declared_source: Option<&'a crate::binding_index::DeclaredValueSource>,
    interpolation_sink: &'a LiftedType,
}

fn check_resource_reference<B: BindingAddress>(
    context: &ResourceReferenceContext<'_, '_, B>,
    reference: ReferencePathRef<'_, B>,
    sink: Option<RefSink<'_>>,
    usage: crate::resource::ResourceRefUsage,
) -> Option<ReferenceFinding> {
    let finding = |kind| ReferenceFinding::new(&context.location, kind);
    let source = match context.bindings.ref_type(reference) {
        RefType::Typed(source) => source,
        RefType::Unchecked => return None,
        RefType::UnknownBinding { .. } => {
            return Some(finding(ReferenceFindingKind::UnknownBinding(
                SecretDiagnosticError::Public(format!(
                    "unknown binding '{}' in reference {}",
                    reference.binding(),
                    reference.to_dot_string()
                )),
            )));
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {
            return None;
        }
        RefType::UnknownAttribute(error) => {
            return Some(finding(ReferenceFindingKind::UnknownAttribute(
                SecretDiagnosticError::Public(error),
            )));
        }
    };
    if sink.is_some() && source.resource_handle_type().is_some() {
        return Some(finding(ReferenceFindingKind::InvalidValue(
            SecretDiagnosticError::Public(format!(
                "'{}' is a resource handle, not a value",
                reference.to_dot_string()
            )),
        )));
    }
    if matches!(reference, ReferencePathRef::Access(path) if !path.segments().is_empty()) {
        return None; // computed postfix checking owns narrowing and sink compatibility
    }
    let sink_type = match sink {
        Some(RefSink::EffectiveType {
            attr_type,
            defs,
            owner,
            ..
        }) => attr_type.in_schema(defs).with_authority(
            &context.bindings.provider_context().custom_types,
            crate::schema::SinkAuthority::Owner(owner),
        ),
        Some(RefSink::Interpolation) => context
            .interpolation_sink
            .assignment_sink(&context.bindings.provider_context().custom_types),
        Some(RefSink::TypeExpr(_)) | None => return None,
    };
    let checked = match context.declared_source {
        Some(declared) => declared.assignment_to_sink(sink_type, context.bindings),
        None => crate::binding_index::DeclaredValueAssignment::from_result(
            source.is_assignable_to_sink(sink_type, context.bindings),
        ),
    };
    match checked {
        crate::binding_index::DeclaredValueAssignment::Assignable
        | crate::binding_index::DeclaredValueAssignment::IncomingBoundaryMismatch => None,
        crate::binding_index::DeclaredValueAssignment::Rejected(
            RefTypeAssignabilityError::Mismatch(mismatch),
        ) => Some(finding(ReferenceFindingKind::TypeMismatch {
            types: mismatch.type_names().clone(),
            path: SecretDiagnosticError::Public(reference.to_dot_string()),
            origin: Some(SecretDiagnosticError::Public(
                mismatch.origin_description(reference),
            )),
        })),
        crate::binding_index::DeclaredValueAssignment::Rejected(
            RefTypeAssignabilityError::Unchecked(reason),
        ) => Some(finding(ReferenceFindingKind::SchemaUnavailable(
            SecretDiagnosticError::Public(reason),
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleCallRefErrorKind {
    InsideSecret {},
    TypeMismatch {
        types: MismatchTypeNames,
        declared: Option<String>,
    },
    UnknownAttribute(RefTypeError),
    DeclarationResolution(StaticTypeResolutionError),
    ProviderSchemaUnavailable {
        reason: String,
    },
}

impl InsideSecret for ModuleCallRefErrorKind {
    fn inside_secret(self) -> Self {
        match self {
            Self::TypeMismatch { types, .. } => Self::TypeMismatch {
                types,
                declared: None,
            },
            _ => Self::InsideSecret {},
        }
    }
}

impl InsideSecret for ModuleCallRefError {
    fn inside_secret(mut self) -> Self {
        self.identity.kind = self.identity.kind.inside_secret();
        self.identity.path = ModuleCallDiagnosticPath::Protected;
        self
    }
}

impl InsideSecret for CompositionRefError {
    fn inside_secret(mut self) -> Self {
        match &mut self {
            Self::ModuleCall(error) => {
                error.path = crate::resource::DiagnosticScope::Secret.path(None);
                error.kind = error.kind.clone().inside_secret();
            }
            Self::Attribute(error) => {
                error.error.path = crate::resource::DiagnosticScope::Secret.path(None);
                error.error.kind = error.error.kind.clone().inside_secret();
            }
        }
        self
    }
}

#[derive(Clone, Copy)]
enum UnknownReferencePolicy {
    Report,
    Defer,
}

/// Whether a missing reference is the optional, absence-selecting operand of
/// `??`. Schema-backed resources are closed shapes: an unknown attribute is a
/// typo, not absence. Upstream exports and composition attribute containers
/// are open shapes whose missing keys may legitimately select the default.
pub fn suppress_coalesce_existence_error(
    error: &RefTypeError,
    usage: crate::resource::ResourceRefUsage,
) -> bool {
    matches!(usage, crate::resource::ResourceRefUsage::CoalesceValue)
        && matches!(
            error,
            RefTypeError::UnknownAttribute {
                target: crate::binding_index::RefTargetKind::LiveComposition,
                ..
            } | RefTypeError::UnknownUpstreamExport { .. }
        )
}

/// Structured module-call reference error shared by CLI and LSP surfaces.
///
/// `Display` is the stable CLI message. The LSP consumes the named fields and
/// never has to recover source identity by parsing that message.
#[derive(Debug, Clone)]
pub struct ModuleCallReferenceIdentity {
    pub call: ModuleCallId,
    pub argument: String,
    path: ModuleCallDiagnosticPath,
    reference_position: Option<usize>,
    binding_use: Option<BindingUseReferenceIdentity>,
    pub kind: ModuleCallRefErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BindingUseReferenceIdentity {
    source_file: SourceFileId,
    span: SourceSpan,
}

impl ModuleCallReferenceIdentity {
    fn has_same_boundary_and_kind(&self, other: &Self) -> bool {
        self.call == other.call && self.argument == other.argument && self.kind == other.kind
    }

    fn represents_same_finding(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }
        self.has_same_boundary_and_kind(other)
            && self.path == other.path
            && matches!(
                (&self.binding_use, &other.binding_use),
                (Some(_), None) | (None, Some(_))
            )
    }

    fn covers(&self, other: &Self) -> bool {
        self == other || (self.has_same_boundary_and_kind(other) && self.path == other.path)
    }
}

impl PartialEq for ModuleCallReferenceIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.has_same_boundary_and_kind(other)
            && match (&self.binding_use, &other.binding_use) {
                (Some(left), Some(right)) => left == right,
                (None, None) => match (self.reference_position, other.reference_position) {
                    (Some(left), Some(right)) => left == right,
                    (None, None) => self.path == other.path,
                    (Some(_), None) | (None, Some(_)) => false,
                },
                (Some(_), None) | (None, Some(_)) => false,
            }
    }
}

impl Eq for ModuleCallReferenceIdentity {}

impl std::hash::Hash for ModuleCallReferenceIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.call.hash(state);
        self.argument.hash(state);
        self.kind.hash(state);
        match &self.binding_use {
            Some(binding_use) => {
                true.hash(state);
                binding_use.hash(state);
            }
            None => {
                false.hash(state);
                match self.reference_position {
                    Some(position) => Some(position).hash(state),
                    None => {
                        Option::<usize>::None.hash(state);
                        self.path.hash(state);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCallRefError {
    call_label: String,
    pub identity: ModuleCallReferenceIdentity,
    /// Parser-authored key span for `argument`.
    pub argument_span: SourceSpan,
}

fn format_module_call_ref_error<B: BindingAddress>(
    f: &mut std::fmt::Formatter<'_>,
    call_label: &str,
    argument: &str,
    path: &DiagnosticPath<B>,
    kind: &ModuleCallRefErrorKind,
) -> std::fmt::Result {
    format_module_call_ref_error_with_path(f, call_label, argument, &path.to_dot_string(), kind)
}

fn format_module_call_ref_error_with_path(
    f: &mut std::fmt::Formatter<'_>,
    call_label: &str,
    argument: &str,
    path: &str,
    kind: &ModuleCallRefErrorKind,
) -> std::fmt::Result {
    match kind {
        ModuleCallRefErrorKind::InsideSecret { .. } => write!(
            f,
            "module call '{call_label}': argument '{argument}': {}",
            crate::resource::INSIDE_SECRET_MESSAGE
        ),
        ModuleCallRefErrorKind::TypeMismatch { types, declared } => write!(
            f,
            "module call '{}': argument '{}': cannot assign {} to '{}': {} ({})",
            call_label,
            argument,
            types.actual(),
            argument,
            types,
            match declared {
                Some(declared) => format!("from {path}, declared {declared}"),
                None => format!("from {path}"),
            },
        ),
        ModuleCallRefErrorKind::UnknownAttribute(error) => {
            write!(f, "module call '{}': {}", call_label, error)
        }
        ModuleCallRefErrorKind::DeclarationResolution(error) => write!(
            f,
            "module call '{call_label}': argument '{argument}': {error}"
        ),
        ModuleCallRefErrorKind::ProviderSchemaUnavailable { reason } => write!(
            f,
            "module call '{call_label}': argument '{argument}': {reason}"
        ),
    }
}

impl std::fmt::Display for ModuleCallRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.identity.path.format_error(
            f,
            &self.call_label,
            &self.identity.argument,
            &self.identity.kind,
        )
    }
}

impl std::error::Error for ModuleCallRefError {}

/// Validate references carried by module-call argument values against binding
/// surfaces and the called module's declared argument types.
///
/// Unknown bindings and enclosing-module argument names are intentionally
/// ignored here: their resolution belongs to the existing scope/module-call
/// passes. This walk only closes attribute-existence gaps for binding targets
/// that have a statically known schema or module signature.
pub fn validate_module_call_argument_ref_types_with_bindings(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    _argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_>,
) -> Vec<ModuleCallRefError> {
    validate_module_call_argument_ref_types_inner(
        module_calls,
        imported_modules,
        bindings,
        UnknownReferencePolicy::Report,
        None,
    )
}

fn validate_module_call_argument_ref_types_inner(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    bindings: &BindingIndex<'_>,
    unknown_policy: UnknownReferencePolicy,
    report: Option<&crate::parser::retained::ExpressionReport>,
) -> Vec<ModuleCallRefError> {
    let mut errors = Vec::new();

    for call in module_calls {
        let call_name = call
            .binding_name
            .as_deref()
            .unwrap_or(call.module_name.as_str());
        for authored in call.arguments_with_source_spans() {
            let argument_name = authored.name();
            let value = authored.argument();
            let argument_span = authored.key_span();
            let declared_sink = imported_modules
                .get(&call.module_name)
                .and_then(|signature| {
                    signature
                        .arguments
                        .iter()
                        .find(|argument| argument.name == argument_name)
                })
                .and_then(|argument| argument.static_type.known());
            if matches!(declared_sink, Some(TypeExpr::Ref(_))) {
                // Resource-typed parameters are structural handle sinks, not
                // ordinary reference-value sinks. The dedicated handle pass
                // owns both matching handles and value-shaped ResourceRefs.
                continue;
            }

            let mut reference_position = 0;
            let value = report
                .and_then(|report| {
                    call.id()
                        .argument_value_location(argument_name)
                        .and_then(|location| report.expression(location).ok())
                })
                .map_or(value.walker_value(), |expression| {
                    expression.assignment_value(value.walker_value())
                });
            let reference_diagnostics = collect_ref_diagnostics_with_sink(
                value,
                declared_sink.map(RefSink::TypeExpr),
                &mut |path, sink, usage, _candidate, reference_findings| {
                    let current_position = reference_position;
                    reference_position += 1;
                    let Some(kind) =
                        composition_ref_error_kind(path, sink, bindings, unknown_policy, usage)
                    else {
                        return;
                    };
                    reference_findings.push(ModuleCallRefError {
                        call_label: call_name.to_string(),
                        identity: ModuleCallReferenceIdentity {
                            call: call.id().clone(),
                            argument: argument_name.to_string(),
                            path: ModuleCallDiagnosticPath::Authored(path.to_owned()),
                            reference_position: Some(current_position),
                            binding_use: None,
                            kind,
                        },
                        argument_span,
                    });
                },
            );
            errors.extend(reference_diagnostics);
        }
    }

    errors
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeParamRefError<B: BindingAddress = RelativeBindingName> {
    pub attribute: String,
    pub path: DiagnosticPath<B>,
    pub kind: ModuleCallRefErrorKind,
}

impl<B: BindingAddress> std::fmt::Display for AttributeParamRefError<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ModuleCallRefErrorKind::InsideSecret { .. } => write!(
                f,
                "attribute '{}': {}",
                self.attribute,
                crate::resource::INSIDE_SECRET_MESSAGE
            ),
            ModuleCallRefErrorKind::TypeMismatch { types, declared } => write!(
                f,
                "attribute '{}': type mismatch: {} ({})",
                self.attribute,
                types,
                match declared {
                    Some(declared) =>
                        format!("from {}, declared {}", self.path.to_dot_string(), declared),
                    None => format!("from {}", self.path.to_dot_string()),
                },
            ),
            ModuleCallRefErrorKind::UnknownAttribute(error) => {
                write!(f, "attribute '{}': {}", self.attribute, error)
            }
            ModuleCallRefErrorKind::DeclarationResolution(error) => {
                write!(f, "attribute '{}': {error}", self.attribute)
            }
            ModuleCallRefErrorKind::ProviderSchemaUnavailable { reason } => {
                write!(f, "attribute '{}': {reason}", self.attribute)
            }
        }
    }
}

impl<B: BindingAddress> std::error::Error for AttributeParamRefError<B> {}

/// A reference-type failure found on one fully expanded composition boundary.
#[derive(Debug, Clone)]
pub enum CompositionRefError {
    ModuleCall(CompositionModuleCallRefError),
    Attribute(CompositionAttributeRefError),
}

/// A call-boundary failure together with structural expansion ancestry used
/// by source-aware consumers such as the LSP.
#[derive(Debug, Clone)]
pub struct CompositionModuleCallRefError {
    pub argument: String,
    pub origin: Box<crate::parser::ArgumentOrigin>,
    pub path: DiagnosticPath<AbsoluteBindingAddress>,
    pub kind: ModuleCallRefErrorKind,
    pub call_path: CompositionCallPath,
    reference_position: usize,
}

impl std::fmt::Display for CompositionModuleCallRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format_module_call_ref_error(
            f,
            &self.call_path.immediate().display_label(),
            &self.argument,
            &self.path,
            &self.kind,
        )
    }
}

/// A module-output declaration failure. The module identity remains attached
/// after expansion so repeated instances collapse to one authored error and
/// CLI output can identify the declaration's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionAttributeRefError {
    pub error: AttributeParamRefError<AbsoluteBindingAddress>,
    pub module_name: String,
    pub module_source: Option<String>,
    pub module_directory: Option<crate::paths::ResolvedPath>,
    pub call_paths: Vec<CompositionCallPath>,
    reference_position: usize,
}

impl CompositionAttributeRefError {
    fn module_label(&self) -> &str {
        self.module_source.as_deref().unwrap_or(&self.module_name)
    }

    fn module_identity(&self) -> CompositionModuleIdentity {
        self.module_directory
            .as_ref()
            .map(|path| CompositionModuleIdentity::Directory(path.identity()))
            .unwrap_or_else(|| {
                CompositionModuleIdentity::Authored(
                    self.module_source
                        .as_ref()
                        .unwrap_or(&self.module_name)
                        .clone(),
                )
            })
    }
}

impl std::fmt::Display for CompositionAttributeRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "module '{}': {}", self.module_label(), self.error)
    }
}

impl std::error::Error for CompositionAttributeRefError {}

impl std::fmt::Display for CompositionRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModuleCall(error) => error.fmt(f),
            Self::Attribute(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CompositionRefError {}

/// Hashable, instance-independent error identity used only for deduplication.
/// Binding/path strings inside [`RefTypeError`] are deliberately omitted: the
/// reference's stable value-walk position identifies the authored occurrence,
/// while expanded bindings differ for every module instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CompositionRefErrorKindKey {
    InsideSecret,
    DeclarationResolution(StaticTypeResolutionError),
    ProviderSchemaUnavailable {
        reason: String,
    },
    TypeMismatch {
        types: MismatchTypeNames,
        declared: Option<String>,
    },
    UnknownAttribute {
        attribute: String,
        known_attributes: Vec<String>,
        target: crate::binding_index::RefTargetKind,
    },
    UnknownStructField {
        field: String,
        struct_name: String,
        known_fields: Vec<String>,
    },
    UnknownUpstreamExport {
        export: String,
        known_exports: Vec<String>,
    },
    InvalidAccess {
        step: crate::resource::PathSegment,
        actual: String,
    },
}

impl From<&ModuleCallRefErrorKind> for CompositionRefErrorKindKey {
    fn from(kind: &ModuleCallRefErrorKind) -> Self {
        match kind {
            ModuleCallRefErrorKind::InsideSecret { .. } => Self::InsideSecret,
            ModuleCallRefErrorKind::DeclarationResolution(error) => {
                Self::DeclarationResolution(error.clone())
            }
            ModuleCallRefErrorKind::ProviderSchemaUnavailable { reason } => {
                Self::ProviderSchemaUnavailable {
                    reason: reason.clone(),
                }
            }
            ModuleCallRefErrorKind::TypeMismatch { types, declared } => Self::TypeMismatch {
                types: types.clone(),
                declared: declared.clone(),
            },
            ModuleCallRefErrorKind::UnknownAttribute(RefTypeError::UnknownAttribute {
                attribute,
                known_attributes,
                target,
                ..
            }) => Self::UnknownAttribute {
                attribute: attribute.clone(),
                known_attributes: known_attributes.clone(),
                target: target.clone(),
            },
            ModuleCallRefErrorKind::UnknownAttribute(RefTypeError::UnknownStructField {
                field,
                struct_name,
                known_fields,
                ..
            }) => Self::UnknownStructField {
                field: field.clone(),
                struct_name: struct_name.clone(),
                known_fields: known_fields.clone(),
            },
            ModuleCallRefErrorKind::UnknownAttribute(RefTypeError::UnknownUpstreamExport {
                export,
                known_exports,
                ..
            }) => Self::UnknownUpstreamExport {
                export: export.clone(),
                known_exports: known_exports.clone(),
            },
            ModuleCallRefErrorKind::UnknownAttribute(RefTypeError::InvalidAccess {
                step,
                actual,
                ..
            }) => Self::InvalidAccess {
                step: step.clone(),
                actual: actual.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CompositionModuleCallErrorKey {
    call_path: CompositionCallPath,
    argument: String,
    reference_position: usize,
    kind: CompositionRefErrorKindKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CompositionModuleIdentity {
    Directory(crate::paths::CanonicalPath),
    Authored(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CompositionAttributeErrorKey {
    module_identity: CompositionModuleIdentity,
    attribute: String,
    reference_position: usize,
    kind: CompositionRefErrorKindKey,
}

fn composition_ref_error_kind<B: BindingAddress>(
    path: ReferencePathRef<'_, B>,
    sink: Option<RefSink<'_>>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    usage: crate::resource::ResourceRefUsage,
) -> Option<ModuleCallRefErrorKind> {
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let declared = sink.and_then(RefSink::as_type_expr)?;
            let declared = match bindings.resolve_declared_type(declared) {
                Ok(declared) => declared,
                Err(error) => return Some(ModuleCallRefErrorKind::DeclarationResolution(error)),
            };
            let sink = lift_type_expr(&declared)?;
            let sink_type = sink.assignment_sink(&bindings.provider_context().custom_types);
            source
                .is_assignable_to_sink(sink_type, bindings)
                .err()
                .map(|error| match error {
                    RefTypeAssignabilityError::Mismatch(mismatch) => {
                        ModuleCallRefErrorKind::TypeMismatch {
                            types: mismatch.type_names().clone(),
                            declared: mismatch.declared_type_name().map(str::to_string),
                        }
                    }
                    RefTypeAssignabilityError::Unchecked(reason) => {
                        ModuleCallRefErrorKind::ProviderSchemaUnavailable {
                            reason: reason.to_string(),
                        }
                    }
                })
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {
            None
        }
        RefType::UnknownAttribute(error) => match unknown_policy {
            UnknownReferencePolicy::Report => Some(ModuleCallRefErrorKind::UnknownAttribute(error)),
            UnknownReferencePolicy::Defer => None,
        },
        RefType::Unchecked | RefType::UnknownBinding { .. } => None,
    }
}

/// Preserve the reference checker's identity and source path when a resolved
/// default reaches a module argument before its composition is constructed.
pub(crate) fn module_argument_reference_error(
    call: &crate::parser::ModuleCall<ParserPlaceholder, AbsoluteBindingAddress>,
    argument: &str,
    path: &crate::resource::AuthoredReferencePath,
    expected: &TypeExpr,
    bindings: &BindingIndex<'_>,
) -> Option<ExpansionBoundaryReferenceErrors> {
    let kind = composition_ref_error_kind(
        path.into(),
        Some(RefSink::TypeExpr(expected)),
        bindings,
        UnknownReferencePolicy::Defer,
        crate::resource::ResourceRefUsage::Required,
    )?;
    let error = ModuleCallRefError {
        call_label: call
            .binding_name
            .as_ref()
            .map_or(call.module_name.as_str(), |name| name.as_str())
            .to_string(),
        identity: ModuleCallReferenceIdentity {
            call: call.id().clone(),
            argument: argument.to_string(),
            path: ModuleCallDiagnosticPath::Authored(path.clone()),
            reference_position: None,
            binding_use: None,
            kind,
        },
        argument_span: call
            .id()
            .argument_key_location(argument)
            .map_or(call.source_span(), |location| location.span()),
    };
    Some(ExpansionBoundaryReferenceErrors {
        errors: vec![ExpansionBoundaryReferenceError::ModuleCall(Box::new(error))],
    })
}

/// Validate resource/binding references at user-function parameter and return
/// boundaries using the same BindingIndex + TypeInSchema relation as module,
/// export, attribute, and provider-attribute references.
///
/// The shared value checker deliberately defers references. This pass supplies
/// the missing binding authority: function-call arguments use their declared
/// parameter type, and function bodies use the declared return type.
pub fn validate_user_function_ref_types_with_bindings<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D>,
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    validate_user_function_ref_types_inner(parsed, bindings, UnknownReferencePolicy::Report)
}

mod user_function_refs {
    use super::*;
    pub(super) enum FunctionRefFinding {
        InsideSecret,
        Reference(crate::resource::AccessPath<RelativeBindingName>, String),
        Boundary(String),
    }
    impl InsideSecret for FunctionRefFinding {
        fn inside_secret(self) -> Self {
            Self::InsideSecret
        }
    }
    impl FunctionRefFinding {
        pub(super) fn into_message(self) -> String {
            match self {
                Self::InsideSecret => crate::resource::INSIDE_SECRET_MESSAGE.into(),
                Self::Reference(_, message) | Self::Boundary(message) => message,
            }
        }
    }
    pub(super) fn check_refs(
        value: &Value,
        sink: Option<RefSink<'_>>,
        context: &str,
        bindings: &BindingIndex<'_>,
        unknown_policy: UnknownReferencePolicy,
        errors: &mut Vec<FunctionRefFinding>,
    ) {
        let reference_diagnostics = collect_ref_diagnostics_with_sink(
            value,
            sink,
            &mut |path, sink, usage, _candidate, reference_findings| {
                let Some(sink) = sink else {
                    return;
                };
                let Some(kind) =
                    composition_ref_error_kind(path, Some(sink), bindings, unknown_policy, usage)
                else {
                    return;
                };
                let message = match kind {
                    ModuleCallRefErrorKind::InsideSecret { .. } => {
                        crate::resource::INSIDE_SECRET_MESSAGE.into()
                    }
                    ModuleCallRefErrorKind::TypeMismatch { types, declared } => format!(
                        "{context}: reference {}: {types} ({})",
                        path.to_dot_string(),
                        declared.map_or_else(
                            || format!("from {}", path.to_dot_string()),
                            |declared| format!(
                                "from {}, declared {declared}",
                                path.to_dot_string()
                            ),
                        )
                    ),
                    ModuleCallRefErrorKind::UnknownAttribute(error) => {
                        format!("{context}: {error}")
                    }
                    ModuleCallRefErrorKind::DeclarationResolution(error) => {
                        format!("{context}: {error}")
                    }
                    ModuleCallRefErrorKind::ProviderSchemaUnavailable { reason } => {
                        format!("{context}: {reason}")
                    }
                };
                reference_findings.push(match path {
                    ReferencePathRef::Access(path) => {
                        FunctionRefFinding::Reference(path.clone(), message)
                    }
                    ReferencePathRef::Binding(_) | ReferencePathRef::BindingAccess { .. } => {
                        FunctionRefFinding::Boundary(message)
                    }
                });
            },
        );
        errors.extend(reference_diagnostics);
    }

    fn declared_result_mismatch(
        source: &TypeExpr,
        sink: &TypeExpr,
        bindings: &BindingIndex<'_>,
    ) -> Result<Option<MismatchTypeNames>, String> {
        let source = bindings
            .resolve_declared_type(source)
            .map_err(|error| error.to_string())?;
        let sink = bindings
            .resolve_declared_type(sink)
            .map_err(|error| error.to_string())?;
        let (Some(source), Some(sink)) = (lift_type_expr(&source), lift_type_expr(&sink)) else {
            return Ok(None);
        };
        let source = crate::schema::TypeInSchema::schemaless(&source);
        let sink = sink.assignment_sink(&bindings.provider_context().custom_types);
        let assignable = bindings
            .check_type_assignment(source, sink)
            .map_err(|error| error.to_string())?;
        Ok((!assignable).then(|| MismatchTypeNames::for_types(*sink, source)))
    }

    fn list_sink(type_expr: &TypeExpr) -> Option<&TypeExpr> {
        match type_expr {
            TypeExpr::List(inner) => Some(inner),
            TypeExpr::Union(members) => exactly_one(members.iter().filter_map(list_sink)),
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::Map(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_) => None,
        }
    }

    fn map_sink<'a>(type_expr: &'a TypeExpr, key: &str) -> Option<&'a TypeExpr> {
        match type_expr {
            TypeExpr::Map(inner) => Some(inner),
            TypeExpr::Struct { fields } => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, field_type)| field_type),
            TypeExpr::Union(members) => {
                exactly_one(members.iter().filter_map(|member| map_sink(member, key)))
            }
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::List(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_) => None,
        }
    }

    pub(super) fn check_function_calls(
        value: &Value,
        result_sink: Option<&TypeExpr>,
        context: Option<&str>,
        bindings: &BindingIndex<'_>,
        unknown_policy: UnknownReferencePolicy,
        errors: &mut Vec<FunctionRefFinding>,
    ) {
        fn check_call(
            name: &str,
            args: &[crate::resource::FunctionArgument<ParserPlaceholder, RelativeBindingName>],
            function: Option<&crate::parser::UserFunction<RelativeBindingName>>,
            result_context: Option<(&TypeExpr, &str)>,
            bindings: &BindingIndex<'_>,
            unknown_policy: UnknownReferencePolicy,
            errors: &mut Vec<FunctionRefFinding>,
        ) {
            if let (Some(source), Some((sink, context))) = (
                function.and_then(|function| function.return_type.as_ref()),
                result_context,
            ) {
                match declared_result_mismatch(source, sink, bindings) {
                    Ok(Some(types)) => errors.push(FunctionRefFinding::Boundary(format!(
                        "{context}: {types} (from function '{name}' return type)"
                    ))),
                    Ok(None) => {}
                    Err(error) => errors.push(FunctionRefFinding::Boundary(format!(
                        "{context}: function '{name}' return type: {error}"
                    ))),
                }
            }
            for (index, argument) in args.iter().enumerate() {
                let parameter_sink = function
                    .and_then(|function| function.params.get(index))
                    .and_then(|parameter| parameter.param_type.as_ref());
                let argument_context = format!("function '{name}': argument {}", index + 1);
                let awaits_module_substitution = argument.pending_parameter().is_some();
                // Explicit handles belong to the parser's in-session argument
                // check. This pass supplies binding authority for values only.
                if let Some(sink) = parameter_sink
                    && !awaits_module_substitution
                    && let Some(value) = argument.value()
                {
                    check_refs(
                        value,
                        Some(RefSink::TypeExpr(sink)),
                        &argument_context,
                        bindings,
                        unknown_policy,
                        errors,
                    );
                }
                check_function_calls(
                    argument.walker_value(),
                    parameter_sink,
                    Some(&argument_context),
                    bindings,
                    unknown_policy,
                    errors,
                );
            }
        }

        match value {
            crate::resource::Value::Deferred(crate::resource::DeferredValue::ModuleIf(
                deferred,
            )) => {
                let [condition, then_value, else_value] = deferred.values();
                check_function_calls(condition, None, context, bindings, unknown_policy, errors);
                for branch in [then_value, else_value] {
                    check_function_calls(
                        branch,
                        result_sink,
                        context,
                        bindings,
                        unknown_policy,
                        errors,
                    );
                }
            }
            crate::resource::Value::Deferred(
                crate::resource::DeferredValue::FunctionCall { name, args, .. }
                | crate::resource::DeferredValue::Unavailable { name, args, .. },
            ) => {
                let function = bindings.user_function(name.as_str());
                check_call(
                    name.as_str(),
                    args,
                    function,
                    result_sink.zip(context),
                    bindings,
                    unknown_policy,
                    errors,
                );
            }
            crate::resource::Value::Deferred(
                crate::resource::DeferredValue::ResolvedFunctionCall { function, args, .. },
            ) => {
                check_call(
                    &function.name,
                    args,
                    Some(function),
                    result_sink.zip(context),
                    bindings,
                    unknown_policy,
                    errors,
                );
            }
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(items)) => {
                for (index, item) in items.iter().enumerate() {
                    let sink = result_sink.and_then(list_sink);
                    let item_context =
                        context.map(|context| format!("{context}, list item {index}"));
                    check_function_calls(
                        item,
                        sink,
                        item_context.as_deref(),
                        bindings,
                        unknown_policy,
                        errors,
                    );
                }
            }
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(entries)) => {
                for (key, item) in entries {
                    let sink = result_sink.and_then(|sink| map_sink(sink, key));
                    let item_context = context.map(|context| format!("{context}, map key '{key}'"));
                    check_function_calls(
                        item,
                        sink,
                        item_context.as_deref(),
                        bindings,
                        unknown_policy,
                        errors,
                    );
                }
            }
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Interpolation(
                parts,
            )) => {
                for part in parts {
                    if let crate::resource::InterpolationPart::Expr(value) = part {
                        check_function_calls(
                            value,
                            Some(&INTERPOLATION_REF_SINK),
                            context,
                            bindings,
                            unknown_policy,
                            errors,
                        );
                    }
                }
            }
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Secret(inner)) => {
                let result = inner.try_inspect(|value| {
                    let mut local = Vec::new();
                    check_function_calls(
                        value,
                        result_sink,
                        context,
                        bindings,
                        unknown_policy,
                        &mut local,
                    );
                    Err::<(), _>(local)
                });
                errors.extend(result.err().into_iter().flatten());
            }
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Access(access)) => {
                check_function_calls(
                    access.base(),
                    None,
                    context,
                    bindings,
                    unknown_policy,
                    errors,
                )
            }
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Coalesce {
                value,
                default,
            }) => {
                check_function_calls(
                    value,
                    result_sink,
                    context,
                    bindings,
                    unknown_policy,
                    errors,
                );
                check_function_calls(
                    default,
                    result_sink,
                    context,
                    bindings,
                    unknown_policy,
                    errors,
                );
            }
            crate::resource::Value::Deferred(
                crate::resource::DeferredValue::IntegerArithmetic(expression),
            ) => {
                check_function_calls(
                    expression.lhs(),
                    Some(&INT_REF_SINK),
                    context,
                    bindings,
                    unknown_policy,
                    errors,
                );
                check_function_calls(
                    expression.rhs(),
                    Some(&INT_REF_SINK),
                    context,
                    bindings,
                    unknown_policy,
                    errors,
                );
            }
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::EnumIdentifier(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::Int(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::Float(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::Bool(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::Duration(_))
            | crate::resource::Value::Concrete(crate::resource::ConcreteValue::StringList(_))
            | crate::resource::Value::Deferred(crate::resource::DeferredValue::ResourceRef {
                ..
            })
            | crate::resource::Value::Deferred(crate::resource::DeferredValue::BindingRef {
                ..
            })
            | crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(_))
            | crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(_)) => {}
        }
    }
}

/// Function boundary findings use the exact reference occurrence recorded by the parser.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, binding_index::BindingIndex, validation::user_function_reference_diagnostics};
/// fn analyze(file: &ParsedFile, bindings: &BindingIndex<'_>) {
///     user_function_reference_diagnostics(file, bindings);
/// }
/// ```
pub fn user_function_reference_diagnostics(
    tree: &crate::parser::AnalysisTree<'_>,
    bindings: &BindingIndex<'_>,
) -> Vec<crate::parser::Located<String>> {
    use user_function_refs::{FunctionRefFinding, check_function_calls, check_refs};
    let mut diagnostics = Vec::new();
    tree.visit_function_values(&mut |input| {
        let value = input.value;
        let expected = input.expected;
        let context = input.context.as_deref();
        let mut findings = Vec::new();
        if input.check_return {
            check_refs(
                value,
                expected.map(RefSink::TypeExpr),
                context.expect("function return context"),
                bindings,
                UnknownReferencePolicy::Report,
                &mut findings,
            );
        }
        check_function_calls(
            value,
            expected,
            context,
            bindings,
            UnknownReferencePolicy::Report,
            &mut findings,
        );
        for finding in findings {
            let (location, message) = match finding {
                FunctionRefFinding::Reference(path, message) => {
                    (tree.reference_for_path(&path).location().clone(), message)
                }
                FunctionRefFinding::Boundary(message) => (input.location().clone(), message),
                FunctionRefFinding::InsideSecret => (
                    input.location().clone(),
                    crate::resource::INSIDE_SECRET_MESSAGE.into(),
                ),
            };
            diagnostics.push(crate::parser::Located::new(location, message));
        }
    });
    diagnostics
}

fn validate_user_function_ref_types_inner<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D>,
    bindings: &BindingIndex<'_>,
    unknown_policy: UnknownReferencePolicy,
) -> Result<(), String> {
    use user_function_refs::{check_function_calls, check_refs};
    let mut errors = Vec::new();

    // Calls can appear in any value-bearing top-level construct. Ordinary
    // references remain owned by their existing validators.
    for resource in parsed.iter_all_resources() {
        let attributes = resource.attributes();
        for value in attributes.values() {
            check_function_calls(value, None, None, bindings, unknown_policy, &mut errors);
        }
    }
    for call in &parsed.module_calls {
        for (_, value) in call.arguments() {
            check_function_calls(
                value.walker_value(),
                None,
                None,
                bindings,
                unknown_policy,
                &mut errors,
            );
        }
    }
    for parameter in &parsed.arguments {
        if let Some(value) = &parameter.default {
            let context = format!("argument '{}' default", parameter.name);
            let expected = Some(&parameter.static_type);
            check_function_calls(
                value,
                expected,
                Some(&context),
                bindings,
                unknown_policy,
                &mut errors,
            );
        }
    }
    for parameter in &parsed.attribute_params {
        if let Some(crate::parser::BoundaryExpression::Value(value)) = &parameter.value {
            let context = format!("attribute '{}'", parameter.name);
            let expected = parameter.static_type.as_ref();
            check_function_calls(
                value,
                expected,
                Some(&context),
                bindings,
                unknown_policy,
                &mut errors,
            );
        }
    }
    for parameter in &parsed.export_params {
        if let Some(value) = parameter.value() {
            let context = format!("export '{}'", parameter.name());
            let expected = parameter.type_expr_opt();
            check_function_calls(
                value,
                expected,
                Some(&context),
                bindings,
                unknown_policy,
                &mut errors,
            );
        }
    }
    for provider in &parsed.providers {
        for value in provider
            .attributes
            .values()
            .chain(provider.default_tags.values())
            .chain(provider.unresolved_attributes.values())
        {
            check_function_calls(value, None, None, bindings, unknown_policy, &mut errors);
        }
    }
    if let Some(backend) = &parsed.backend {
        for value in backend.attributes.values() {
            check_function_calls(value, None, None, bindings, unknown_policy, &mut errors);
        }
    }
    for state_block in &parsed.state_blocks {
        if let crate::parser::StateBlock::Import { id, .. } = state_block {
            check_function_calls(id, None, None, bindings, unknown_policy, &mut errors);
        }
    }
    for value in parsed.variables.values() {
        check_function_calls(value, None, None, bindings, unknown_policy, &mut errors);
    }

    for function in parsed.user_functions.values() {
        for parameter in &function.params {
            if let Some(value) = &parameter.default {
                let context = format!(
                    "function '{}': parameter '{}' default",
                    function.name, parameter.name
                );
                check_function_calls(
                    value,
                    parameter.param_type.as_ref(),
                    Some(&context),
                    bindings,
                    unknown_policy,
                    &mut errors,
                );
            }
        }
        for (_, value, _) in &function.local_lets {
            check_function_calls(value, None, None, bindings, unknown_policy, &mut errors);
        }
        check_refs(
            &function.body.value,
            function.return_type.as_ref().map(RefSink::TypeExpr),
            &format!("function '{}': return value", function.name),
            bindings,
            unknown_policy,
            &mut errors,
        );
        let context = format!("function '{}': return value", function.name);
        check_function_calls(
            &function.body.value,
            function.return_type.as_ref(),
            Some(&context),
            bindings,
            unknown_policy,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors
            .into_iter()
            .map(user_function_refs::FunctionRefFinding::into_message)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// Validate every fully expanded module-call boundary exactly once.
///
/// Both halves of the boundary live on [`crate::resource::LiveComposition`]:
/// typed call arguments and typed output attributes. The shared
/// [`BindingIndex`] is built only after all nested compositions have been
/// prefixed into the root parse, so unannotated forwarded outputs can inherit
/// their resource-schema type through arbitrarily deep composition chains.
///
/// Saved compositions cannot enter this live validation seam:
///
/// ```compile_fail
/// use carina_core::binding_index::BindingIndex;
/// use carina_core::resource::SavedComposition;
/// use carina_core::validation::validate_composition_ref_types_with_bindings;
/// fn validate(saved: &[SavedComposition], bindings: &BindingIndex<'_>) {
///     let _ = validate_composition_ref_types_with_bindings(saved, bindings);
/// }
/// ```
pub fn validate_composition_ref_types_with_bindings<P: PlaceholderKind>(
    compositions: &[crate::resource::LiveComposition<P>],
    bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
) -> Vec<CompositionRefError> {
    let mut errors = Vec::new();

    for composition in compositions {
        let call_path = composition.call_path();
        let call = call_path.immediate();
        for (argument_name, argument) in &composition.signature.arguments {
            let (declared_type, origin) = argument.validation_metadata();
            if matches!(declared_type.known(), Some(TypeExpr::Ref(_))) {
                // Checked below by the structural handle-boundary pass; a
                // resource attribute value must never be accepted merely
                // because its identity type is related to the resource.
                continue;
            }
            let mut reference_position = 0;
            let declared_sink = declared_type.known().map(RefSink::TypeExpr);
            let reference_diagnostics = collect_ref_diagnostics_with_sink(
                argument.value(),
                declared_sink,
                &mut |path, sink, usage, _candidate, reference_findings| {
                    let current_position = reference_position;
                    reference_position += 1;
                    let Some(kind) = composition_ref_error_kind(
                        path,
                        sink,
                        bindings,
                        UnknownReferencePolicy::Report,
                        usage,
                    ) else {
                        return;
                    };
                    reference_findings.push(CompositionRefError::ModuleCall(
                        CompositionModuleCallRefError {
                            argument: argument_name.clone(),
                            origin: Box::new(origin),
                            path: crate::resource::DiagnosticScope::Public.path(path.to_owned()),
                            kind,
                            call_path: call_path.clone(),
                            reference_position: current_position,
                        },
                    ));
                },
            );
            errors.extend(reference_diagnostics);
        }

        for (attribute_name, attribute) in &composition.signature.attributes {
            if attribute.source_argument().is_some() {
                // The module-local declaration is checked against the
                // argument's declared type before expansion. Re-checking the
                // substituted caller value here duplicates call-boundary
                // errors and incorrectly blames the module output.
                continue;
            }
            let Some(value) = attribute.value() else {
                continue;
            };
            let mut reference_position = 0;
            let declared_sink = attribute
                .declared_type()
                .and_then(|static_type| static_type.known())
                .map(RefSink::TypeExpr);
            let reference_diagnostics = collect_ref_diagnostics_with_sink(
                &value,
                declared_sink,
                &mut |path, sink, usage, _candidate, reference_findings| {
                    let current_position = reference_position;
                    reference_position += 1;
                    let Some(kind) = composition_ref_error_kind(
                        path,
                        sink,
                        bindings,
                        UnknownReferencePolicy::Report,
                        usage,
                    ) else {
                        return;
                    };
                    reference_findings.push(CompositionRefError::Attribute(
                        CompositionAttributeRefError {
                            error: AttributeParamRefError {
                                attribute: attribute_name.clone(),
                                path: crate::resource::DiagnosticScope::Public
                                    .path(path.to_owned()),
                                kind,
                            },
                            module_name: call.module_name.clone(),
                            module_source: call.module_source.clone(),
                            module_directory: call.module_directory.clone(),
                            call_paths: vec![call_path.clone()],
                            reference_position: current_position,
                        },
                    ));
                },
            );
            errors.extend(reference_diagnostics);
        }
    }

    let mut seen_calls = HashSet::new();
    let mut attribute_indices = std::collections::HashMap::new();
    let mut deduplicated = Vec::new();
    for error in errors {
        match error {
            CompositionRefError::ModuleCall(error) => {
                let key = CompositionModuleCallErrorKey {
                    call_path: error.call_path.clone(),
                    argument: error.argument.clone(),
                    reference_position: error.reference_position,
                    kind: (&error.kind).into(),
                };
                if seen_calls.insert(key) {
                    deduplicated.push(CompositionRefError::ModuleCall(error));
                }
            }
            CompositionRefError::Attribute(error) => {
                let key = CompositionAttributeErrorKey {
                    module_identity: error.module_identity(),
                    attribute: error.error.attribute.clone(),
                    reference_position: error.reference_position,
                    kind: (&error.error.kind).into(),
                };
                if let Some(index) = attribute_indices.get(&key).copied() {
                    let CompositionRefError::Attribute(existing) = &mut deduplicated[index] else {
                        unreachable!("attribute deduplication indexes only attribute errors")
                    };
                    for call_path in error.call_paths {
                        if !existing.call_paths.contains(&call_path) {
                            existing.call_paths.push(call_path);
                        }
                    }
                } else {
                    attribute_indices.insert(key, deduplicated.len());
                    deduplicated.push(CompositionRefError::Attribute(error));
                }
            }
        }
    }
    deduplicated
}

/// Identity shared by anonymous resources with the same missing schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AnonymousResourceSchemaKey {
    provider: String,
    resource_type: String,
    provider_instance: Option<String>,
}

/// A grouped diagnostic for anonymous resources whose schema is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnonymousResourceSchemaError {
    key: AnonymousResourceSchemaKey,
    count: usize,
}

impl std::fmt::Display for AnonymousResourceSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let provider = if self.key.provider.is_empty() {
            "<default>"
        } else {
            &self.key.provider
        };
        let resource_type = if self.key.provider.is_empty() {
            self.key.resource_type.clone()
        } else {
            format!("{}.{}", self.key.provider, self.key.resource_type)
        };

        if self.count == 1 {
            write!(f, "anonymous {resource_type} resource needs ")?;
        } else {
            write!(
                f,
                "{} anonymous {resource_type} resources need ",
                self.count
            )?;
        }

        match &self.key.provider_instance {
            Some(instance) => write!(f, "provider instance '{instance}' of '{provider}'")?,
            None => write!(f, "provider '{provider}'")?,
        }
        write!(
            f,
            ", which is not declared or could not be loaded; declare provider '{provider}' so its schema is available"
        )
    }
}

impl std::error::Error for AnonymousResourceSchemaError {}

/// Report every anonymous managed resource whose identity is still pending
/// because its provider schema is unavailable.
///
/// A known provider with no matching resource type is deliberately excluded:
/// [`validate_resources`] reports that case as `Unknown resource type` for
/// bound and anonymous resources alike. This check covers the distinct case
/// where the provider was not declared or could not be loaded, and therefore
/// no schema exists from which to derive an anonymous identity.
pub fn validate_anonymous_resource_schemas<'a, P: PlaceholderKind + 'a, B: BindingAddress + 'a>(
    resources: impl IntoIterator<Item = &'a GenericResource<ModuleSource, P, B>>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
) -> Vec<AnonymousResourceSchemaError> {
    group_anonymous_resource_schema_errors(
        resources.into_iter().filter_map(|resource| {
            anonymous_resource_schema_key(resource, registry, known_providers)
        }),
    )
}

fn anonymous_resource_schema_key<P: PlaceholderKind, B: BindingAddress>(
    resource: &GenericResource<ModuleSource, P, B>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
) -> Option<AnonymousResourceSchemaKey> {
    (resource.binding.is_none()
        && resource.id.identity_str().is_none()
        && registry.get_for(resource).is_none()
        && !known_providers.contains(&resource.id.provider))
    .then(|| AnonymousResourceSchemaKey {
        provider: resource.id.provider.clone(),
        resource_type: resource.id.resource_type.clone(),
        provider_instance: resource.id.provider_instance.clone(),
    })
}

fn group_anonymous_resource_schema_errors(
    keys: impl IntoIterator<Item = AnonymousResourceSchemaKey>,
) -> Vec<AnonymousResourceSchemaError> {
    let mut grouped = IndexMap::<AnonymousResourceSchemaKey, usize>::new();
    for key in keys {
        *grouped.entry(key).or_default() += 1;
    }

    grouped
        .into_iter()
        .map(|(key, count)| AnonymousResourceSchemaError { key, count })
        .collect()
}

#[derive(Clone, Copy)]
enum ValidatableKind {
    Resource,
    DataSource,
}

/// A resource-admission failure retains its schema error and authored owner.
/// Display is the execution-facing presentation; diagnostic clients consume
/// the typed error and location without parsing that presentation.
#[derive(Debug)]
pub struct ResourceAdmissionFinding {
    human: String,
    detail: ResourceAdmissionDetail,
    location: Option<crate::parser::DeclarationLocation>,
}

#[derive(Debug)]
enum ResourceAdmissionDetail {
    Schema {
        resource: crate::resource::ResourceId,
        attribute: Option<String>,
        error: TypeError,
        mismatch: Option<Box<ReferenceMismatch>>,
    },
    Arithmetic(Box<crate::parser::Located<IntegerArithmeticTypeFinding>>),
    Iterable(Box<crate::parser::Located<DeferredForIterableTypeDiagnostic>>),
    UnavailableSchema(AnonymousResourceSchemaError),
    Message,
}

impl ResourceAdmissionFinding {
    /// Add CLI module context as presentation without changing the finding.
    pub fn with_context(mut self, prefix: &str) -> Self {
        self.human = self
            .human
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(|line| format!("{prefix}: {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        self
    }
    fn schema(
        resource: &crate::resource::ResourceId,
        attribute: Option<&str>,
        error: &TypeError,
    ) -> Self {
        Self {
            human: format!("{resource}: {error}"),
            detail: ResourceAdmissionDetail::Schema {
                resource: resource.clone(),
                attribute: attribute.map(str::to_owned),
                error: error.clone(),
                mismatch: None,
            },
            location: None,
        }
    }
    pub fn type_error(&self) -> Option<&TypeError> {
        match &self.detail {
            ResourceAdmissionDetail::Schema { error, .. } => Some(error),
            _ => None,
        }
    }
    pub fn resource_id(&self) -> Option<&crate::resource::ResourceId> {
        match &self.detail {
            ResourceAdmissionDetail::Schema { resource, .. } => Some(resource),
            _ => None,
        }
    }
    pub fn location(&self) -> Option<&crate::parser::DeclarationLocation> {
        self.location.as_ref()
    }
    pub fn message(&self) -> String {
        match &self.detail {
            ResourceAdmissionDetail::Schema {
                mismatch: Some(mismatch),
                ..
            } => mismatch.message(),
            ResourceAdmissionDetail::Schema {
                error: TypeError::Reference { error, .. },
                ..
            } => match error.as_ref() {
                SecretDiagnosticError::Public(admission::AdmissionReferenceError::Arithmetic(
                    finding,
                )) => finding.detail().to_string(),
                _ => error.to_string(),
            },
            ResourceAdmissionDetail::Schema { error, .. } => error.to_string(),
            ResourceAdmissionDetail::Arithmetic(finding) => finding.detail().to_string(),
            ResourceAdmissionDetail::Iterable(finding) => finding.detail().to_string(),
            ResourceAdmissionDetail::UnavailableSchema(error) => error.to_string(),
            ResourceAdmissionDetail::Message => self.human.clone(),
        }
    }
}
impl From<String> for ResourceAdmissionFinding {
    fn from(human: String) -> Self {
        Self {
            human,
            detail: ResourceAdmissionDetail::Message,
            location: None,
        }
    }
}
impl From<crate::parser::Located<IntegerArithmeticTypeFinding>> for ResourceAdmissionFinding {
    fn from(finding: crate::parser::Located<IntegerArithmeticTypeFinding>) -> Self {
        Self {
            human: finding.to_string(),
            location: Some(finding.location().clone()),
            detail: ResourceAdmissionDetail::Arithmetic(Box::new(finding)),
        }
    }
}
impl From<crate::parser::Located<DeferredForIterableTypeDiagnostic>> for ResourceAdmissionFinding {
    fn from(finding: crate::parser::Located<DeferredForIterableTypeDiagnostic>) -> Self {
        Self {
            human: finding.to_string(),
            location: Some(finding.location().clone()),
            detail: ResourceAdmissionDetail::Iterable(Box::new(finding)),
        }
    }
}
impl From<AnonymousResourceSchemaError> for ResourceAdmissionFinding {
    fn from(error: AnonymousResourceSchemaError) -> Self {
        Self {
            human: error.to_string(),
            location: None,
            detail: ResourceAdmissionDetail::UnavailableSchema(error),
        }
    }
}

impl std::fmt::Display for ResourceAdmissionFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.human)
    }
}
impl std::error::Error for ResourceAdmissionFinding {}

#[derive(Default)]
struct ResourceAdmissionFindings(Vec<ResourceAdmissionFinding>);
impl ResourceAdmissionFindings {
    fn push(&mut self, finding: impl Into<ResourceAdmissionFinding>) {
        self.0.push(finding.into());
    }
    fn extend<T: Into<ResourceAdmissionFinding>>(&mut self, findings: impl IntoIterator<Item = T>) {
        self.0.extend(findings.into_iter().map(Into::into));
    }
}

struct ResourceValidationContext<'a> {
    registry: &'a SchemaRegistry,
    known_providers: &'a HashSet<String>,
    provider_context: &'a ProviderContext,
    all_errors: &'a mut ResourceAdmissionFindings,
    diagnostics: &'a mut dyn deferred_types::ProviderTypeCheckSink,
    admissions:
        Option<&'a HashMap<crate::resource::ResourceId, crate::schema::ResourceValueValidation>>,
}

fn validate_resource_values<P: PlaceholderKind, B: BindingAddress>(
    id: &crate::resource::ResourceId,
    quoted_string_attrs: &HashSet<String>,
    attributes: &IndexMap<String, GenericValue<P, B>>,
    schema: Option<crate::schema::OwnerBoundResourceSchema<'_>>,
    kind: ValidatableKind,
    context: &mut ResourceValidationContext<'_>,
) {
    match schema {
        Some(schema) => {
            let mut classification_errors = Vec::new();
            let classified = crate::resource::knowledge::collect_classified_attributes(
                attributes.iter().map(|(name, value)| (name.clone(), value)),
                &crate::resource::knowledge::KnowledgeContext::validation(),
                |name, error| classification_errors.push((name.to_owned(), error)),
            );
            let checked;
            let values = if let Some(values) =
                context.admissions.and_then(|reports| reports.get(id))
            {
                values
            } else {
                checked = publish_admission_diagnostics(
                    deferred_types::TypeCheckSession::run(context.provider_context, |session| {
                        match kind {
                            ValidatableKind::Resource => {
                                admission::attributes::admit_validation_attributes::<
                                    admission::kind::ResourceAttribute,
                                    _,
                                    _,
                                >(
                                    schema,
                                    id,
                                    &classified,
                                    quoted_string_attrs,
                                    None,
                                    session,
                                    None,
                                    None,
                                )
                            }
                            ValidatableKind::DataSource => {
                                admission::attributes::admit_validation_attributes::<
                                    admission::kind::DataSourceAttribute,
                                    _,
                                    _,
                                >(
                                    schema,
                                    id,
                                    &classified,
                                    quoted_string_attrs,
                                    None,
                                    session,
                                    None,
                                    None,
                                )
                            }
                        }
                    }),
                    context.diagnostics,
                );
                &checked
            };
            for (error, path, symbolic) in values.diagnostic_errors() {
                if path.is_some_and(|(name, _)| values.is_retained_failure(name)) {
                    continue;
                }
                let rendered_symbolically = context.admissions.is_some() && symbolic.owns_path();
                if !rendered_symbolically {
                    context.all_errors.push(ResourceAdmissionFinding::schema(
                        id,
                        path.map(|(name, _)| name),
                        error,
                    ));
                }
            }
            context.all_errors.extend(
                classification_errors
                    .into_iter()
                    .map(|(name, error)| format!("{id}.{name}: {error}")),
            );
        }
        None => {
            let provider = id.provider.as_str();
            let resource_type = id.resource_type.as_str();

            // No matching-kind entry. Skip if provider is not loaded —
            // schemas are simply not available, not a configuration error.
            if !provider.is_empty() && !context.known_providers.contains(provider) {
                return;
            }
            let has_managed = context.registry.has_managed(provider, resource_type);
            let has_data_source = context.registry.has_data_source(provider, resource_type);
            let kind_label = if provider.is_empty() {
                resource_type.to_string()
            } else {
                format!("{provider}.{resource_type}")
            };

            match kind {
                ValidatableKind::DataSource if has_managed => {
                    // `read` used against a managed-only type
                    context.all_errors.push(format!(
                        "{} is a managed resource, not a data source. Remove the `read` keyword:\n  let <name> = {} {{ }}",
                        kind_label, kind_label
                    ));
                }
                ValidatableKind::Resource if has_data_source => {
                    // No `read` against a data-source-only type
                    context.all_errors.push(format!(
                        "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                        kind_label, kind_label
                    ));
                }
                _ => {
                    context
                        .all_errors
                        .push(format!("Unknown resource type: {kind_label}"));
                }
            }
        }
    }
}

/// Validate resources against their schemas.
///
/// Two-sided check: a `read` resource requires a `DataSource` registry entry,
/// and a non-`read` resource requires a `Managed` registry entry. If the
/// wrong-kind entry is present (e.g. `read` against a managed-only type),
/// emit a kind-specific error explaining the mismatch.
pub fn validate_resources<
    E,
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    B: BindingAddress,
>(
    parsed: &crate::parser::File<E, P, T, D, B>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
    provider_context: &ProviderContext,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<(), String> {
    validate_resources_from_report(
        parsed,
        registry,
        known_providers,
        provider_context,
        None,
        diagnostics,
    )
}

fn validate_resources_from_report<
    E,
    P: PlaceholderKind,
    T: PlaceholderKind,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    B: BindingAddress,
>(
    parsed: &crate::parser::File<E, P, T, D, B>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
    provider_context: &ProviderContext,
    admissions: Option<
        &HashMap<crate::resource::ResourceId, crate::schema::ResourceValueValidation>,
    >,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<(), String> {
    let findings = resource_values_report(
        parsed,
        registry,
        known_providers,
        provider_context,
        admissions,
        diagnostics,
    );
    if findings.is_empty() {
        Ok(())
    } else {
        Err(findings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn resource_values_report<
    E,
    P: PlaceholderKind,
    T: PlaceholderKind,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    B: BindingAddress,
>(
    parsed: &crate::parser::File<E, P, T, D, B>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
    provider_context: &ProviderContext,
    admissions: Option<
        &HashMap<crate::resource::ResourceId, crate::schema::ResourceValueValidation>,
    >,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Vec<ResourceAdmissionFinding> {
    let mut all_errors = ResourceAdmissionFindings::default();
    // A standalone module receives its providers from the caller, so schema
    // availability cannot be decided until its resources are expanded into a
    // root configuration. Keep every other schema/type check below active for
    // modules; only defer this provider-availability diagnostic.
    if !is_module_definition(parsed) {
        all_errors.extend(group_anonymous_resource_schema_errors(
            parsed
                .iter_all_resources()
                .filter_map(|resource| match resource {
                    ResourceRef::Resource(resource) => {
                        anonymous_resource_schema_key(resource, registry, known_providers)
                    }
                    ResourceRef::Deferred { resource, .. } => anonymous_resource_schema_key(
                        resource.as_resource(),
                        registry,
                        known_providers,
                    ),
                    ResourceRef::LiveComposition(_) | ResourceRef::DataSource(_) => None,
                }),
        ));
    }

    // Classify per kind via the typed `ResourceRef` arms instead of
    // runtime `is_virtual()` / `is_data_source()` calls (carina#3180 /
    // #3181). compositions are post-apply attribute containers and have no
    // schema to validate against, so they are silently filtered. Managed
    // and data sources route to the same schema-lookup body but render
    // different kind-mismatch diagnostics when the registry entry of the
    // *opposite* kind exists.
    {
        let mut context = ResourceValidationContext {
            registry,
            known_providers,
            provider_context,
            all_errors: &mut all_errors,
            diagnostics,
            admissions,
        };
        for rref in parsed.iter_all_resources() {
            // A deferred for-expression template body is always managed —
            // `for` bodies never carry `read` / composition.
            match rref {
                ResourceRef::LiveComposition(_) => {}
                ResourceRef::DataSource(resource) => validate_resource_values(
                    &resource.id,
                    &resource.quoted_string_attrs,
                    &resource.attributes,
                    registry.get_for_data_source(resource),
                    ValidatableKind::DataSource,
                    &mut context,
                ),
                ResourceRef::Resource(resource) => {
                    let schema = registry.get_for(resource);
                    validate_resource_values(
                        &resource.id,
                        &resource.quoted_string_attrs,
                        &resource.attributes,
                        schema,
                        ValidatableKind::Resource,
                        &mut context,
                    );
                    if let Some(schema) = schema
                        && let Err(errors) =
                            crate::resource::resolve_ignore_changes(resource, &schema)
                    {
                        context.all_errors.extend(
                            errors
                                .into_iter()
                                .map(|error| format!("{}: {}", resource.id, error)),
                        );
                    }
                }
                ResourceRef::Deferred { resource, .. } => {
                    let schema = registry.get_for(resource.as_resource());
                    validate_resource_values(
                        resource.id(),
                        resource.quoted_string_attrs(),
                        resource.attributes(),
                        schema,
                        ValidatableKind::Resource,
                        &mut context,
                    );
                    if let Some(schema) = schema
                        && let Err(errors) =
                            crate::resource::resolve_ignore_changes(resource.as_resource(), &schema)
                    {
                        context.all_errors.extend(
                            errors
                                .into_iter()
                                .map(|error| format!("{}: {}", resource.id(), error)),
                        );
                    }
                }
            }
        }
    }
    all_errors.0
}

/// Validate that resource references have compatible types.
///
/// For example, if `ipv4_ipam_pool_id` expects `IpamPoolId` type,
/// a reference like `vpc.vpc_id` (which is `AwsResourceId`) should be an error.
/// Resource reference findings remain distinct from a rejected analysis input.
#[derive(Debug, thiserror::Error)]
pub enum ResourceRefTypeValidationError {
    #[error(transparent)]
    AnalysisInvariant(#[from] crate::parser::AnalysisTreeError),
    #[error("{0}")]
    Validation(String),
}

pub fn validate_resource_ref_types<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    _argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_, B>,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<(), ResourceRefTypeValidationError> {
    let report = resource_admission_report(judged, bindings, diagnostics)?;
    validation_messages_result(
        report
            .references
            .0
            .into_iter()
            .map(|finding| finding.to_string())
            .collect(),
    )
}

pub fn resource_reference_findings<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<Vec<ResourceAdmissionFinding>, crate::parser::AnalysisTreeError> {
    Ok(resource_admission_report(judged, bindings, diagnostics)?
        .references
        .0)
}

fn validation_messages_result(errors: Vec<String>) -> Result<(), ResourceRefTypeValidationError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ResourceRefTypeValidationError::Validation(
            errors.join("\n"),
        ))
    }
}

/// Validate resource/data-source attributes once, then render value and
/// reference findings from that report for CLI callers.
pub fn validate_resource_admissions<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
    known_providers: &HashSet<String>,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<(), ResourceRefTypeValidationError> {
    let findings = resource_admission_findings(judged, bindings, known_providers, diagnostics)?;
    validation_messages_result(
        findings
            .into_iter()
            .map(|finding| finding.to_string())
            .collect(),
    )
}

/// Typed counterpart to the execution-facing string wrapper above.
pub fn resource_admission_findings<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
    known_providers: &HashSet<String>,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<Vec<ResourceAdmissionFinding>, crate::parser::AnalysisTreeError> {
    let report = resource_admission_report(judged, bindings, diagnostics)?;
    let mut findings = resource_values_report(
        judged.file(),
        judged.schemas(),
        known_providers,
        bindings.provider_context(),
        Some(&report.attributes),
        diagnostics,
    );
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file())?;
    let mut resources = HashMap::new();
    for resource in tree.project_resources() {
        // Preserve the former find() policy if an earlier validation phase
        // has retained multiple declarations with the same identity.
        resources.entry(resource.id()).or_insert(resource);
    }
    for finding in &mut findings {
        if let ResourceAdmissionDetail::Schema {
            resource: id,
            attribute,
            ..
        } = &finding.detail
            && let Some(resource) = resources.get(id)
        {
            finding.location = Some(
                attribute
                    .as_deref()
                    .map(|name| resource.attribute_key_location(name))
                    .unwrap_or_else(|| resource.type_location())
                    .clone(),
            );
        }
    }
    findings.extend(report.references.0);
    Ok(findings)
}

fn publish_admission_diagnostics<T>(
    result: deferred_types::ProviderTypeCheckResult<T>,
    sink: &mut (impl deferred_types::ProviderTypeCheckSink + ?Sized),
) -> T {
    result.publish(sink)
}

struct ResourceAdmissionReport {
    attributes: HashMap<crate::resource::ResourceId, crate::schema::ResourceValueValidation>,
    references: ResourceAdmissionFindings,
}

/// Recover the lexical type of a bare value that became an entire reference
/// attribute during expansion. The runtime reference stays absolute; only its
/// module-body type retains the declaration and the original source together.
fn module_resource_ref_declaration<'a, E, D, B: BindingAddress>(
    parsed: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    resource_id: &crate::resource::ResourceId,
    attribute: &str,
    value: &GenericValue<ParserPlaceholder, B>,
    bindings: &BindingIndex<'_, B>,
) -> Option<(&'a str, crate::binding_index::DeclaredValueSource)> {
    let GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path: source_path }) =
        value
    else {
        return None;
    };
    parsed.bare_binding_uses().records().iter().find_map(|record| {
        let owns_resource = match &record.authored.owner {
            crate::parser::BindingUseOwner::Resource(address) => {
                address.resource_id() == resource_id
            }
            crate::parser::BindingUseOwner::ModuleArgument {
                call: _call,
                argument: _argument,
                value_span: _value_span,
            } => false,
        };
        let path = record.authored.path.segments();
        if !owns_resource
            || path.len() != 1
            || !matches!(path.first(), Some(crate::parser::AttributePathSegment::Attribute(name)) if name == attribute)
        {
            return None;
        }
        match &record.source {
            crate::parser::BareBindingSource::Value {
                value: source,
                declared_type,
            } => {
                if source.as_ref() != value {
                    return None;
                }
                let RefType::Typed(source) = bindings.ref_type(source_path) else { return None; };
                let source = source.with_value_declaration(declared_type.as_deref()?, bindings)?;
                Some((record.authored.name.as_str(), source))
            }
            crate::parser::BareBindingSource::Structural(_handle) => None,
        }
    })
}

fn resource_admission_report<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    judged: &crate::parser::retained::JudgedFile<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
    diagnostics: &mut dyn deferred_types::ProviderTypeCheckSink,
) -> Result<ResourceAdmissionReport, crate::parser::AnalysisTreeError> {
    let parsed = judged.file();
    let registry = judged.schemas();
    let mut all_errors = ResourceAdmissionFindings::default();
    let mut attributes = HashMap::new();
    let mut coverage = admission::WalkCoverage::default();
    let inference_bindings = inference::bindings_from_parsed(parsed);
    let tree = crate::parser::AnalysisTree::try_from_file(parsed)?;

    // Validate declarations directly instead of relying on a later resource
    // attribute to happen to expose an expression. Keep the authored
    // operator keys so a let substituted into an attribute is not reported a
    // second time by the schema-sink pass below.
    let mut declaration_arithmetic_findings = HashSet::new();
    for finding in check_file_integer_arithmetic_values(judged, bindings) {
        let message = finding.to_string();
        declaration_arithmetic_findings.insert((finding.location().clone(), message.clone()));
        all_errors.push(finding);
    }

    publish_admission_diagnostics(
        crate::validation::deferred_types::TypeCheckSession::run(
            bindings.provider_context(),
            |session| {
                // Match the main-branch project walk. Parser-side deferred read
                // templates are checked at expansion, not admitted here.
                for resource in tree.project_resources() {
                    let Some(schema) = resource.schema(registry) else {
                        continue;
                    };
                    let attrs = resource.attributes();
                    let inputs = match admission::attributes::ResourceAttributeInputs::for_resource(
                        resource,
                        judged.report(),
                    ) {
                        Ok(inputs) => inputs,
                        Err(error) => {
                            all_errors.push(error.to_string());
                            continue;
                        }
                    };
                    let classified = inputs.classify(
                        &crate::resource::knowledge::KnowledgeContext::validation(),
                        |_, _| {},
                    );
                    let values = admission::attributes::resource_attribute_admissions(
                        resource,
                        parsed,
                        schema,
                        &classified,
                        resource.quoted_string_attrs(),
                        &inputs,
                        bindings,
                        session,
                        registry,
                        &mut coverage,
                    );
                    let location = resource.diagnostic_label();
                    for (attribute, path, error) in values.symbolic_errors() {
                        let start = all_errors.0.len();
                        match error {
                            TypeError::Reference { error, .. } => match error.as_ref() {
                                SecretDiagnosticError::Public(
                                    admission::AdmissionReferenceError::Arithmetic(finding),
                                ) => {
                                    if !declaration_arithmetic_findings.contains(&(
                                        finding.location().clone(),
                                        finding.to_string(),
                                    )) {
                                        all_errors.push(finding.to_string());
                                    }
                                }
                                SecretDiagnosticError::Public(
                                    admission::AdmissionReferenceError::Computed(
                                        DeferredAccessTypeError::TypeMismatch { types, origin },
                                    ),
                                ) => {
                                    let message = reference_type_mismatch_message(
                                        Some(attribute),
                                        types,
                                        Some(origin),
                                    );
                                    all_errors.push(format!("{location}: {message}"));
                                }
                                SecretDiagnosticError::Public(
                                    admission::AdmissionReferenceError::Computed(
                                        DeferredAccessTypeError::InvalidAccess { error },
                                    ),
                                ) => {
                                    all_errors.push(format!("{location}: {error}"));
                                }
                                SecretDiagnosticError::Public(
                                    admission::AdmissionReferenceError::UnknownBinding { .. }
                                    | admission::AdmissionReferenceError::Path(_),
                                ) => all_errors.push(format!("{location}: {error}")),
                                SecretDiagnosticError::Public(_)
                                | SecretDiagnosticError::InsideSecret => {
                                    all_errors.push(format!("{location}.{attribute}: {error}"))
                                }
                            },
                            TypeError::TypeMismatch { types } => {
                                let message = ReferenceMismatch::for_attribute(
                                    &attrs[attribute],
                                    path,
                                    attribute,
                                    types,
                                )
                                .assignment_message();
                                all_errors.push(format!("{location}: {message}"));
                            }
                            error => all_errors.push(format!("{location}.{attribute}: {error}")),
                        }
                        for finding in &mut all_errors.0[start..] {
                            finding.detail = ResourceAdmissionDetail::Schema {
                                resource: resource.id().clone(),
                                attribute: Some(attribute.to_string()),
                                error: error.clone(),
                                mismatch: match error {
                                    TypeError::TypeMismatch { types } => {
                                        Some(Box::new(ReferenceMismatch::for_attribute(
                                            &attrs[attribute],
                                            path,
                                            attribute,
                                            types,
                                        )))
                                    }
                                    _ => None,
                                },
                            };
                            finding.location =
                                Some(resource.attribute_key_location(attribute).clone());
                        }
                    }
                    attributes.insert(resource.id().clone(), values);
                }
            },
        ),
        diagnostics,
    );

    if parsed.deferred_for_sites().next().is_some() {
        all_errors.extend(validate_deferred_for_iterable_types_with_bindings(
            &crate::parser::AnalysisTree::try_from_file(parsed)?,
            registry,
            &inference_bindings,
            bindings.provider_context(),
        ));
    }

    Ok(ResourceAdmissionReport {
        attributes,
        references: all_errors,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferredIterableKind {
    List,
    Map,
}

#[derive(Debug, Clone, PartialEq)]
enum DeferredIterableShape {
    List,
    Map,
    NonIterable(TypeExpr),
}

impl From<crate::parser::retained::CollectionKind> for DeferredIterableShape {
    fn from(kind: crate::parser::retained::CollectionKind) -> Self {
        match kind {
            crate::parser::retained::CollectionKind::List => Self::List,
            crate::parser::retained::CollectionKind::Map => Self::Map,
        }
    }
}

#[cfg(test)]
fn deferred_iterable_shape(
    value: &Value,
    bindings: &inference::InferenceBindings<ParserPlaceholder>,
    schemas: &SchemaRegistry,
) -> Result<DeferredIterableShape, crate::static_type::InferenceIssue> {
    match value {
        GenericValue::Concrete(crate::resource::ConcreteValue::List(_)) => {
            Ok(DeferredIterableShape::List)
        }
        GenericValue::Concrete(crate::resource::ConcreteValue::Map(_)) => {
            Ok(DeferredIterableShape::Map)
        }
        _ => inference::infer_projected_type(
            value,
            bindings,
            schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .map(iterable_shape_from_type),
    }
}

fn deferred_iterable_shape_for_phase(
    expression: &crate::parser::retained::SinkJudgedExpr<'_>,
) -> Result<DeferredIterableShape, crate::static_type::InferenceIssue> {
    match expression.collection_kind() {
        Some(kind) => Ok(kind.into()),
        None => expression
            .static_type()
            .cloned()
            .map(iterable_shape_from_type),
    }
}

fn iterable_shape_from_type(inferred: TypeExpr) -> DeferredIterableShape {
    match inferred {
        TypeExpr::List(_) => DeferredIterableShape::List,
        TypeExpr::Map(_) => DeferredIterableShape::Map,
        non_iterable => DeferredIterableShape::NonIterable(non_iterable),
    }
}

/// Static type failure for an iterable retained as a
/// [`crate::parser::DeferredForExpression`].
///
/// The file/line identity lets the LSP reuse the exact CLI validator while
/// anchoring only diagnostics owned by the current editor buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredForIterableTypeDiagnostic {
    pub expression: String,
    pub message: String,
}

impl std::fmt::Display for DeferredForIterableTypeDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "deferred-for iterable `{}`: {}",
            self.expression, self.message
        )
    }
}

/// Module and read expansion need collection membership before apply.
/// Missing validation facts alone never prove an apply-only requirement.
pub fn validate_module_iteration_shape<P: PlaceholderKind, B: BindingAddress>(
    value: &crate::resource::knowledge::Classification<'_, P, B>,
) -> Result<(), crate::resource::knowledge::PhaseViolation<B>> {
    value.module_iteration_violation().map_or(Ok(()), Err)
}

/// Validate every deferred `for` iterable against its binding pattern.
///
/// Coalesce operands are checked independently: even while the left remains
/// unresolved, a statically known scalar fallback cannot be allowed to reach
/// apply for a loop that requires a list or map.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::{ParsedFile, ProviderContext}, schema::SchemaRegistry, validation::validate_deferred_for_iterable_types};
/// fn analyze(file: &ParsedFile, context: &ProviderContext) {
///     validate_deferred_for_iterable_types(file, &SchemaRegistry::new(), context);
/// }
/// ```
pub fn validate_deferred_for_iterable_types<D, B: BindingAddress>(
    tree: &crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &SchemaRegistry,
    provider_context: &ProviderContext,
) -> Vec<crate::parser::Located<DeferredForIterableTypeDiagnostic>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    validate_deferred_for_iterable_types_with_bindings(
        tree,
        schemas,
        &tree.inference_bindings(),
        provider_context,
    )
}

fn validate_deferred_for_iterable_types_with_bindings<
    E,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    tree: &crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B, E>,
    _schemas: &SchemaRegistry,
    _bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    provider_context: &ProviderContext,
) -> Vec<crate::parser::Located<DeferredForIterableTypeDiagnostic>> {
    let mut diagnostics = Vec::new();
    for deferred in tree.for_iterables() {
        let expected = match deferred.binding() {
            ForBinding::Simple(_) | ForBinding::Indexed(_, _) => DeferredIterableKind::List,
            ForBinding::Map(_, _) => DeferredIterableKind::Map,
        };
        let judged = match tree
            .file()
            .expression_report()
            .instances_at(deferred.location())
        {
            Ok(judged) => judged,
            Err(error) => {
                diagnostics.push(crate::parser::Located::new(
                    deferred.location().clone(),
                    DeferredForIterableTypeDiagnostic {
                        expression: crate::value::format_value(deferred.iterable()),
                        message: error.to_string(),
                    },
                ));
                continue;
            }
        };
        for judged in judged {
            validate_deferred_iterable_operand(
                DeferredIterableValidationContext {
                    expected,
                    judged,
                    provider_context,
                    location: deferred.location(),
                    expression: deferred.iterable(),
                },
                &mut diagnostics,
            );
        }
    }
    diagnostics
}

/// Unknown roots in deferred iterables are owned by their authored occurrence.
pub fn validate_for_iterable_bindings<D, B: BindingAddress>(
    tree: &crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
) -> Vec<crate::parser::Located<crate::parser::ParseError>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut findings = Vec::new();
    let mut in_scope: Vec<_> = tree.binding_names().iter().cloned().collect();
    in_scope.sort();
    for deferred in tree
        .for_iterables()
        .iter()
        .filter(|deferred| deferred.checks_scope())
    {
        let mut roots = std::collections::BTreeMap::new();
        let mut record = |key, path: crate::resource::DiagnosticPath<B>| {
            let existing = roots.entry(key).or_insert_with(|| path.clone());
            if path.is_secret() {
                *existing = path;
            }
        };
        deferred
            .iterable()
            .visit_resource_refs_for_diagnostics(&mut |reference, _| {
                record(reference.identity().binding_key(), reference.binding_path());
            });
        deferred
            .iterable()
            .visit_binding_refs_for_diagnostics(&mut |reference| {
                record(reference.identity(), reference.diagnostic_path());
            });
        for (root, diagnostic_path) in roots {
            if root.is_in_names(tree.binding_names()) {
                continue;
            }
            let occurrences: Vec<_> = deferred
                .references()
                .iter()
                .filter(|reference| reference.binding_matches(|binding| binding == root))
                .map(|reference| reference.prefix_location())
                .collect();
            let locations = if occurrences.is_empty() {
                vec![deferred.location()]
            } else {
                occurrences
            };
            for location in locations {
                findings.push(crate::parser::Located::new(
                    location.clone(),
                    crate::parser::ParseError::undefined_identifier(
                        diagnostic_path.to_dot_string(),
                        location.span().start_line(),
                        in_scope.clone(),
                    ),
                ));
            }
        }
    }
    findings
}

impl std::fmt::Display for DeferredIterableKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List => f.write_str("list"),
            Self::Map => f.write_str("map"),
        }
    }
}

struct DeferredIterableValidationContext<'a, B: BindingAddress> {
    expected: DeferredIterableKind,
    provider_context: &'a ProviderContext,
    judged: &'a crate::parser::retained::JudgedExpr,
    location: &'a crate::parser::DeclarationLocation,
    expression: &'a GenericValue<ParserPlaceholder, B>,
}

impl<B: BindingAddress> Copy for DeferredIterableValidationContext<'_, B> {}

impl<B: BindingAddress> Clone for DeferredIterableValidationContext<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

fn validate_deferred_iterable_operand<B: BindingAddress>(
    context: DeferredIterableValidationContext<'_, B>,
    diagnostics: &mut Vec<crate::parser::Located<DeferredForIterableTypeDiagnostic>>,
) {
    if let Some(operands) = context.judged.coalesce_operands() {
        let mut operand_context = context;
        operand_context.judged = &operands[0];
        validate_deferred_iterable_operand(operand_context, diagnostics);
        operand_context.judged = &operands[1];
        validate_deferred_iterable_operand(operand_context, diagnostics);
        return;
    }

    let sink = match context.expected {
        DeferredIterableKind::List => TypeExpr::List(Box::new(TypeExpr::String)),
        DeferredIterableKind::Map => TypeExpr::Map(Box::new(TypeExpr::String)),
    };
    let judged = context.judged.for_sink(&sink, context.provider_context);
    let inferred = match deferred_iterable_shape_for_phase(&judged) {
        Ok(inferred) => Some(inferred),
        Err(crate::static_type::InferenceIssue::Failed(
            error @ (inference::InferenceError::DirectoryConstraint(_)
            | inference::InferenceError::InsideSecret { .. }
            | inference::InferenceError::SecretAssignmentMismatch { .. }
            | inference::InferenceError::MissingEvidence(_)
            | inference::InferenceError::IncompatibleCoalesceTypes { .. }
            | inference::InferenceError::HeterogeneousCollection { .. }),
        )) => {
            diagnostics.push(deferred_iterable_diagnostic(context, error.to_string()));
            None
        }
        Err(crate::static_type::InferenceIssue::Failed(
            error @ inference::InferenceError::InvalidIntegerArithmeticOperand { .. },
        )) => {
            diagnostics.push(deferred_iterable_diagnostic(context, error.to_string()));
            None
        }
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::UnknownType { .. }
            | inference::InferenceError::UnknownBinding { .. }
            | inference::InferenceError::UnknownAttribute { .. }
            | inference::InferenceError::InvalidAccess { .. }
            | inference::InferenceError::CyclicVirtualBinding { .. },
        ))
        | Err(crate::static_type::InferenceIssue::NotInferable(
            crate::static_type::NotInferableReason::MissingValue
            | crate::static_type::NotInferableReason::ValueDependent { .. }
            | crate::static_type::NotInferableReason::PlaceholderTypeUnavailable { .. }
            | crate::static_type::NotInferableReason::NonInferableBinding { .. }
            | crate::static_type::NotInferableReason::SchemaUnavailable { .. }
            | crate::static_type::NotInferableReason::CustomTypeSchemaUnavailable { .. },
        )) => None,
    };

    let Some(inferred) = inferred else {
        return;
    };
    match (context.expected, inferred) {
        (DeferredIterableKind::List, DeferredIterableShape::List)
        | (DeferredIterableKind::Map, DeferredIterableShape::Map) => {}
        (DeferredIterableKind::List, DeferredIterableShape::Map) => {
            diagnostics.push(deferred_iterable_diagnostic(
                context,
                "expected list iterable, got map".to_string(),
            ));
        }
        (DeferredIterableKind::Map, DeferredIterableShape::List) => {
            diagnostics.push(deferred_iterable_diagnostic(
                context,
                "expected map iterable, got list".to_string(),
            ));
        }
        (expected, DeferredIterableShape::NonIterable(type_expr)) => {
            diagnostics.push(deferred_iterable_diagnostic(
                context,
                format!("expected {expected} iterable, got {type_expr}"),
            ));
        }
    }
}

fn deferred_iterable_diagnostic<B: BindingAddress>(
    context: DeferredIterableValidationContext<'_, B>,
    message: String,
) -> crate::parser::Located<DeferredForIterableTypeDiagnostic> {
    crate::parser::Located::new(
        context.location.clone(),
        DeferredForIterableTypeDiagnostic {
            expression: crate::value::format_value(context.expression),
            message,
        },
    )
}

fn finding_messages<T: std::fmt::Display>(findings: impl IntoIterator<Item = T>) -> String {
    findings
        .into_iter()
        .map(|finding| finding.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn parameter_findings_result<T: std::fmt::Display>(findings: Vec<T>) -> Result<(), String> {
    if findings.is_empty() {
        Ok(())
    } else {
        Err(finding_messages(findings))
    }
}

/// Validate that attribute parameter ResourceRef values name attributes on
/// their referenced schemas, and that plain refs with a simple custom type
/// annotation are type-compatible.
///
/// For example, `attributes { role_arn: iam_role_arn = role.role_name }` should
/// be rejected because `role_name` is `String`, not `IamRoleArn`.
pub fn validate_attribute_param_ref_types_with_bindings<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    bindings: &BindingIndex<'_, B>,
) -> Result<(), String> {
    parameter_findings_result(
        attribute_param_ref_findings(attribute_params, bindings)
            .into_iter()
            .map(|finding| finding.into_parts().1)
            .collect(),
    )
}

pub fn attribute_param_ref_findings<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    bindings: &BindingIndex<'_, B>,
) -> Vec<crate::parser::Located<ReferenceFinding>> {
    let mut findings = Vec::new();
    for param in attribute_params {
        let mut references = Vec::new();
        check_attribute_param_refs(
            &param.name,
            param
                .value
                .as_ref()
                .and_then(crate::parser::BoundaryExpression::value),
            param.static_type.as_ref(),
            param.judgement().ok(),
            bindings,
            UnknownReferencePolicy::Report,
            &mut references,
        );
        findings.extend(references.into_iter().map(|finding| {
            crate::parser::Located::new(param.type_check_location().clone(), finding)
        }));
    }
    findings
}

pub fn validate_consumer_attribute_param_ref_types_with_bindings<B: BindingAddress>(
    attribute_params: &[crate::parser::ConsumerAttributeParameter<B>],
    bindings: &BindingIndex<'_, B>,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for param in attribute_params {
        let expected_type = match &param.static_type {
            Some(StaticType::Known(resolved)) => Some(resolved.type_expr()),
            Some(StaticType::Unchecked(_reason)) => None,
            None => None,
        };
        check_attribute_param_refs(
            &param.name,
            param
                .value
                .as_ref()
                .and_then(crate::parser::BoundaryExpression::value),
            expected_type,
            param.judgement().ok(),
            bindings,
            UnknownReferencePolicy::Report,
            &mut errors,
        );
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(finding_messages(errors))
    }
}

fn validate_attribute_param_ref_types_inner<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in attribute_params {
        check_attribute_param_refs(
            &param.name,
            param
                .value
                .as_ref()
                .and_then(crate::parser::BoundaryExpression::value),
            param.static_type.as_ref(),
            param.judgement().ok(),
            bindings,
            unknown_policy,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(finding_messages(errors))
    }
}

fn check_attribute_param_refs<P: PlaceholderKind, B: BindingAddress>(
    param_name: &str,
    value: Option<&GenericValue<P, B>>,
    expected_type: Option<&TypeExpr>,
    expression: Option<&crate::parser::retained::JudgedExpr>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    errors: &mut Vec<ReferenceFinding>,
) {
    let Some(value) = value else {
        return;
    };
    let value = expression.map_or(value, |expression| expression.assignment_value(value));
    let reference_diagnostics = collect_ref_diagnostics_with_sink(
        value,
        expected_type.map(RefSink::TypeExpr),
        &mut |path, sink, usage, _candidate, reference_findings| {
            check_attribute_param_ref(
                param_name,
                sink.and_then(RefSink::as_type_expr),
                path,
                bindings,
                unknown_policy,
                usage,
                reference_findings,
            );
        },
    );
    errors.extend(reference_diagnostics);
}

fn check_attribute_param_ref<B: BindingAddress>(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: ReferencePathRef<'_, B>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    usage: crate::resource::ResourceRefUsage,
    errors: &mut Vec<ReferenceFinding>,
) {
    let location = ReferenceFindingLocation::Attribute(param_name.into());
    let finding = |kind| ReferenceFinding::new(&location, kind);
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let Some(type_expr) = expected_type else {
                return;
            };
            let resolved = match bindings.resolve_declared_type(type_expr) {
                Ok(resolved) => resolved,
                Err(error) => {
                    errors.push(finding(ReferenceFindingKind::InvalidValue(
                        SecretDiagnosticError::Public(error.to_string()),
                    )));
                    return;
                }
            };
            if let TypeExpr::Ref(expected) = &resolved {
                errors.push(finding(ReferenceFindingKind::InvalidValue(
                    SecretDiagnosticError::Public(
                        HandleSinkError::reference_for_handle(expected, &source).to_string(),
                    ),
                )));
                return;
            }
            let Some(sink) = lift_type_expr(&resolved) else {
                return;
            };
            let sink_type = sink.assignment_sink(&bindings.provider_context().custom_types);
            let mismatch = match source.is_assignable_to_sink(sink_type, bindings) {
                Ok(()) => return,
                Err(RefTypeAssignabilityError::Mismatch(mismatch)) => mismatch,
                Err(RefTypeAssignabilityError::Unchecked(reason)) => {
                    errors.push(finding(ReferenceFindingKind::SchemaUnavailable(
                        SecretDiagnosticError::Public(reason),
                    )));
                    return;
                }
            };
            let origin = Some(SecretDiagnosticError::Public(
                mismatch.origin_description(path),
            ));
            errors.push(finding(ReferenceFindingKind::TypeMismatch {
                types: mismatch.type_names().clone(),
                path: SecretDiagnosticError::Public(path.to_dot_string()),
                origin,
            }));
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {}
        RefType::UnknownAttribute(error)
            if matches!(unknown_policy, UnknownReferencePolicy::Report) =>
        {
            errors.push(finding(ReferenceFindingKind::UnknownAttribute(
                SecretDiagnosticError::Public(error),
            )));
        }
        RefType::UnknownAttribute(_) | RefType::Unchecked | RefType::UnknownBinding { .. } => {}
    }
}

/// Validate export parameter values that are ResourceRef against their declared
/// TypeExpr by looking up the referenced attribute's schema type.
///
/// This catches mismatches like `exports { x: list(bool) = [vpc.vpc_id] }` where
/// `vpc_id` is a string attribute but the export declares `bool`.
pub fn validate_export_param_ref_types_with_bindings<P: PlaceholderKind, B: BindingAddress>(
    export_params: &[crate::parser::InferredExportParam<P, B>],
    bindings: &BindingIndex<'_, B>,
) -> Result<(), Vec<ExportBoundaryError>> {
    let mut errors = validate_export_handle_boundaries(export_params)
        .err()
        .unwrap_or_default();
    errors.extend(
        export_params
            .iter()
            .flat_map(|param| {
                collect_export_param_ref_type_errors(
                    param,
                    bindings,
                    UnknownReferencePolicy::Report,
                )
                .into_iter()
                .map(|message| ExportBoundaryError::new(param, message))
            })
            .collect::<Vec<_>>(),
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[derive(Debug, Clone)]
pub struct ExportBoundaryError {
    pub name: String,
    source: crate::parser::DeclarationSource,
    message: String,
}

impl ExportBoundaryError {
    fn new<P: PlaceholderKind, B: BindingAddress>(
        param: &crate::parser::InferredExportParam<P, B>,
        message: String,
    ) -> Self {
        Self {
            name: param.name.clone(),
            source: param.declaration_source(),
            message,
        }
    }

    pub fn diagnostic<'a, D, B: BindingAddress>(
        &'a self,
        tree: &crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    ) -> crate::parser::Located<&'a Self>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let declaration = tree
            .syntax()
            .declarations()
            .iter()
            .find(|declaration| {
                declaration.kind() == crate::config_loader::DeclarationKind::Export
                    && declaration.name() == self.name
                    && declaration.location().file().path() == self.source_file()
                    && self.source_span().contains(declaration.location().span())
            })
            .expect("export boundary provenance validated by analysis");
        crate::parser::Located::new(declaration.location().clone(), self)
    }

    pub fn source_file(&self) -> &std::path::Path {
        &self.source.file
    }

    pub fn source_span(&self) -> crate::parser::SourceSpan {
        self.source.span
    }
}

impl std::fmt::Display for ExportBoundaryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExportBoundaryError {}

/// Reject resource handles at the state export boundary after inference has
/// retained whether the effective type was declared or inferred.
///
/// A handle-shaped right-hand side wins over an unrelated value annotation
/// during inference, so this check also rejects declarations such as
/// `exported: String = resource` instead of silently erasing the structural
/// operand when [`crate::parser::InferredExportParam`] becomes value-only.
pub fn validate_export_handle_boundaries<P: PlaceholderKind, B: BindingAddress>(
    export_params: &[crate::parser::InferredExportParam<P, B>],
) -> Result<(), Vec<ExportBoundaryError>> {
    let mut errors = Vec::new();

    for param in export_params {
        let (provenance, resource_type) = match &param.static_type {
            ExportStaticType::Declared(TypeExpr::Ref(resource_type)) => ("", resource_type),
            ExportStaticType::Inferred(StaticType::Known(TypeExpr::Ref(resource_type))) => {
                ("inferred ", resource_type)
            }
            ExportStaticType::Declared(
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::UnloadedSchemaType(_)
                | TypeExpr::List(_)
                | TypeExpr::Map(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_)
                | TypeExpr::Union(_),
            )
            | ExportStaticType::Inferred(StaticType::Known(
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::UnloadedSchemaType(_)
                | TypeExpr::List(_)
                | TypeExpr::Map(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_)
                | TypeExpr::Union(_),
            ))
            | ExportStaticType::Inferred(StaticType::Unchecked(_)) => continue,
        };

        errors.push(ExportBoundaryError::new(
            param,
            format!(
                "export '{}': {provenance}resource handle {resource_type} cannot cross a state boundary; export an identity value such as {resource_type}.Id instead",
                param.name,
            ),
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_export_param_ref_types_inner<
    P: PlaceholderKind,
    B: BindingAddress,
    E: crate::parser::ExportParamLike<P, B>,
>(
    export_params: &[E],
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in export_params {
        errors.extend(collect_export_param_ref_type_errors(
            param,
            bindings,
            unknown_policy,
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn collect_export_param_ref_type_errors<
    P: PlaceholderKind,
    B: BindingAddress,
    E: crate::parser::ExportParamLike<P, B>,
>(
    param: &E,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(value) = param.value() else {
        return errors;
    };
    if param.inference_failed() {
        return errors;
    }
    let sink = param.type_expr_opt().map(RefSink::TypeExpr);
    let value = param
        .judgement()
        .ok()
        .map_or(value, |expression| expression.assignment_value(value));
    let reference_diagnostics = collect_ref_diagnostics_with_sink(
        value,
        sink,
        &mut |path, sink, usage, _candidate, reference_findings| {
            check_export_ref(
                param.name(),
                sink.and_then(RefSink::as_type_expr),
                path,
                bindings,
                unknown_policy,
                usage,
                reference_findings,
            );
        },
    );
    errors.extend(
        reference_diagnostics
            .into_iter()
            .map(|finding| finding.to_string()),
    );
    errors
}

fn check_export_ref<B: BindingAddress>(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: ReferencePathRef<'_, B>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    usage: crate::resource::ResourceRefUsage,
    errors: &mut Vec<ReferenceFinding>,
) {
    let location = ReferenceFindingLocation::Export(param_name.into());
    let finding = |kind| ReferenceFinding::new(&location, kind);
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let Some(type_expr) = expected_type else {
                return;
            };
            let resolved = match bindings.resolve_declared_type(type_expr) {
                Ok(resolved) => resolved,
                Err(error) => {
                    errors.push(finding(ReferenceFindingKind::InvalidValue(
                        SecretDiagnosticError::Public(error.to_string()),
                    )));
                    return;
                }
            };
            let Some(sink) = lift_type_expr(&resolved) else {
                return;
            };
            let sink_type = sink.assignment_sink(&bindings.provider_context().custom_types);
            let mismatch = match source.is_assignable_to_sink(sink_type, bindings) {
                Ok(()) => return,
                Err(RefTypeAssignabilityError::Mismatch(mismatch)) => mismatch,
                Err(RefTypeAssignabilityError::Unchecked(reason)) => {
                    errors.push(finding(ReferenceFindingKind::SchemaUnavailable(
                        SecretDiagnosticError::Public(reason),
                    )));
                    return;
                }
            };
            let origin = mismatch
                .declared_type_name()
                .map(|_| SecretDiagnosticError::Public(mismatch.origin_description(path)));
            errors.push(finding(ReferenceFindingKind::TypeMismatch {
                types: mismatch.type_names().clone(),
                path: SecretDiagnosticError::Public(path.to_dot_string()),
                origin,
            }));
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {}
        RefType::UnknownAttribute(error)
            if matches!(unknown_policy, UnknownReferencePolicy::Report) =>
        {
            errors.push(finding(ReferenceFindingKind::UnknownAttribute(
                SecretDiagnosticError::Public(error),
            )));
        }
        RefType::UnknownAttribute(_) | RefType::Unchecked | RefType::UnknownBinding { .. } => {}
    }
}

/// Validate only the reference relations that expansion relies on.
///
/// Ordinary resource attribute references deliberately stay out of this
/// mandatory witness: recovery commands may skip provider/resource validation.
/// Missing schemas and unknown attributes are likewise deferred here, while a
/// known source and sink still use BindingIndex + TypeInSchema as the sole
/// declared-type assignability authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpansionBoundaryReferenceError {
    Attribute(String),
    Export(String),
    ModuleCall(Box<ModuleCallRefError>),
    Function(String),
}

impl std::fmt::Display for ExpansionBoundaryReferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Attribute(message) | Self::Export(message) | Self::Function(message) => {
                formatter.write_str(message)
            }
            Self::ModuleCall(error) => error.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionBoundaryReferenceErrors {
    errors: Vec<ExpansionBoundaryReferenceError>,
}

impl ExpansionBoundaryReferenceErrors {
    pub fn iter(&self) -> impl Iterator<Item = &ExpansionBoundaryReferenceError> {
        self.errors.iter()
    }

    pub(crate) fn without_covered_module_calls(
        self,
        findings: &[ModuleCallFinding],
    ) -> Option<Self> {
        let errors = self
            .errors
            .into_iter()
            .filter(|error| match error {
                ExpansionBoundaryReferenceError::ModuleCall(error) => !findings
                    .iter()
                    .any(|finding| finding.message().covers_reference_type(error)),
                ExpansionBoundaryReferenceError::Attribute(_message)
                | ExpansionBoundaryReferenceError::Export(_message)
                | ExpansionBoundaryReferenceError::Function(_message) => true,
            })
            .collect::<Vec<_>>();
        (!errors.is_empty()).then_some(Self { errors })
    }

    #[cfg(test)]
    pub(crate) fn function_for_test(message: &str) -> Self {
        Self {
            errors: vec![ExpansionBoundaryReferenceError::Function(
                message.to_string(),
            )],
        }
    }
}

impl std::fmt::Display for ExpansionBoundaryReferenceErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            error.fmt(formatter)?;
        }
        Ok(())
    }
}

impl std::error::Error for ExpansionBoundaryReferenceErrors {}

pub(crate) fn validate_expansion_boundary_ref_types_with_bindings<
    E: crate::parser::ExportParamLike,
>(
    parsed: &crate::parser::File<E>,
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    bindings: &BindingIndex<'_>,
) -> Result<(), ExpansionBoundaryReferenceErrors> {
    let mut errors = Vec::new();

    if let Err(message) = validate_attribute_param_ref_types_inner(
        &parsed.attribute_params,
        bindings,
        UnknownReferencePolicy::Defer,
    ) {
        errors.extend(
            message
                .lines()
                .map(|line| ExpansionBoundaryReferenceError::Attribute(line.to_string())),
        );
    }
    if let Err(message) = validate_export_param_ref_types_inner(
        &parsed.export_params,
        bindings,
        UnknownReferencePolicy::Defer,
    ) {
        errors.extend(
            message
                .lines()
                .map(|line| ExpansionBoundaryReferenceError::Export(line.to_string())),
        );
    }
    errors.extend(
        validate_module_call_argument_ref_types_inner(
            &parsed.module_calls,
            imported_modules,
            bindings,
            UnknownReferencePolicy::Defer,
            Some(parsed.expression_report()),
        )
        .into_iter()
        .map(|error| ExpansionBoundaryReferenceError::ModuleCall(Box::new(error))),
    );
    if let Err(message) =
        validate_user_function_ref_types_inner(parsed, bindings, UnknownReferencePolicy::Defer)
    {
        errors.extend(
            message
                .lines()
                .map(|line| ExpansionBoundaryReferenceError::Function(line.to_string())),
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ExpansionBoundaryReferenceErrors { errors })
    }
}

/// Check if an AttributeType is string-compatible (can accept a string value).
///
/// `defs` is threaded so any `Ref` receiver is peeled before shape
/// dispatch. A `Ref` target that resolves to a non-string shape
/// (typically a `Struct`) returns false, same as before — but a
/// `Ref` pointing at a string-compatible alias now answers correctly
/// instead of silently rejecting.
pub fn is_string_compatible_type(
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> bool {
    is_string_compatible_type_on_path(attr_type, defs, &[])
}

fn all_resolved_union_members_on_path<'a>(
    members: &crate::schema::UnionMembers<'a, crate::schema::Compiled>,
    visited_refs: &mut Vec<&'a str>,
    mut predicate: impl FnMut(
        crate::schema::ResolvedUnionMember<'a, crate::schema::Compiled>,
        &[&'a str],
    ) -> bool,
) -> bool {
    let mut saw_member = false;
    let found_incompatible = members.any_on_path(visited_refs, |member, member_path| {
        saw_member = true;
        !predicate(member, member_path)
    });
    saw_member && !found_incompatible
}

fn is_string_compatible_type_on_path<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    visited_refs: &[&'a str],
) -> bool {
    let mut current_path = visited_refs.to_vec();
    let resolved = attr_type.resolve_refs_with_defs_on_path(defs, &mut current_path);
    let resolved_attr = resolved.as_attr();
    match resolved_attr
        .shape_ref_free()
        .expect("resolve_refs_with_defs_on_path must peel every top-level Ref")
    {
        Shape::UnloadedSchemaType(_) => false,
        Shape::Reference { .. } | Shape::String { .. } | Shape::Enum { .. } => true,
        Shape::Union => {
            let members = crate::schema::union_members_with_defs(resolved_attr, defs)
                .expect("Shape::Union must expose union members internally");
            all_resolved_union_members_on_path(
                &members,
                &mut current_path,
                |member, member_path| {
                    is_string_compatible_type_on_path(member.as_attr(), defs, member_path)
                },
            )
        }
        Shape::Int { .. }
        | Shape::Float { .. }
        | Shape::Bool
        | Shape::Duration
        | Shape::List { .. }
        | Shape::Map { .. }
        | Shape::Struct { .. } => false,
    }
}

/// Check that a root configuration does not contain `arguments` blocks.
///
/// `arguments` is a module-input declaration: it belongs on the module side
/// of a module boundary and is paired with `use` on the caller side. In a
/// root configuration there is no caller to pass values, so the block has
/// no meaning — its `default` would silently become a de-facto root
/// variable, which is not a documented feature (issue #2198).
///
/// A directory loaded via the CLI may be either a root config or a module
/// the user is validating in isolation. We only flag the misplaced block
/// when a `backend` or `provider` block is also present, since both are
/// root-only constructs and unambiguously identify a root configuration.
pub fn validate_no_arguments_in_root<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_root = parsed.backend.is_some() || !parsed.providers.is_empty();
    if !parsed.arguments.is_empty() && is_root {
        return Err(
            "arguments blocks are only valid inside module definitions, not in root configurations.".to_string(),
        );
    }
    Ok(())
}

fn unknown_type_resolution_error(
    authored_name: &str,
    config: &ProviderContext,
) -> StaticTypeResolutionError {
    crate::parser::unknown_custom_type_error(authored_name, config)
}
fn resolve_dotted_unresolved(
    path: &ResourceTypePath,
    config: &ProviderContext,
) -> Result<TypeExpr, StaticTypeResolutionError> {
    if config.has_resource_type(&path.provider, &path.resource_type) {
        return Ok(TypeExpr::Ref(path.clone()));
    }

    let mut authored_parts = path
        .resource_type
        .split('.')
        .map(String::from)
        .collect::<Vec<_>>();
    let Some(kind) = authored_parts.pop() else {
        return Err(unknown_type_resolution_error(&path.to_string(), config));
    };
    let matches = config
        .known_custom_types()
        .iter()
        .filter(|registered| {
            registered.matches_authored_path(&path.provider, &authored_parts, &kind)
        })
        .collect::<Vec<_>>();
    if let Some(first) = matches.first() {
        return crate::parser::resolve_custom_type_identity(first.identity(), config)?
            .ok_or_else(|| unknown_type_resolution_error(&path.to_string(), config));
    }

    if config.custom_types.may_defer(Some(&path.provider)) {
        return Ok(TypeExpr::DottedUnresolved(path.clone()));
    }
    Err(unknown_type_resolution_error(&path.to_string(), config))
}

/// Resolve parser-produced dotted type paths against an enriched provider
/// context, recursing through every nested type-expression container.
pub fn resolve_type_expr(
    ty: &TypeExpr,
    config: &ProviderContext,
) -> Result<TypeExpr, StaticTypeResolutionError> {
    match ty {
        TypeExpr::DottedUnresolved(path) => resolve_dotted_unresolved(path, config),
        TypeExpr::UnloadedSchemaType(unloaded) => {
            crate::parser::resolve_custom_type_identity(unloaded.identity(), config)?
                .ok_or_else(|| unknown_type_resolution_error(&ty.to_string(), config))
        }
        TypeExpr::List(inner) => {
            resolve_type_expr(inner, config).map(|t| TypeExpr::List(Box::new(t)))
        }
        TypeExpr::Map(inner) => {
            resolve_type_expr(inner, config).map(|t| TypeExpr::Map(Box::new(t)))
        }
        TypeExpr::Union(members) => members
            .iter()
            .map(|m| resolve_type_expr(m, config))
            .collect::<Result<Vec<_>, _>>()
            .map(TypeExpr::Union),
        TypeExpr::Struct { fields } => fields
            .iter()
            .map(|(name, field_ty)| {
                resolve_type_expr(field_ty, config).map(|resolved| (name.clone(), resolved))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|fields| TypeExpr::Struct { fields }),
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::Ref(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::StringLiteral(_) => Ok(ty.clone()),
    }
}

/// A parameter type after dotted names have been resolved against the
/// provider registry. This classification is used only to check an argument;
/// the argument itself decides whether a parameter is bound as a handle or a
/// value.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedParameterType {
    Handle(crate::parser::ResourceTypePath),
    Value(TypeExpr),
}

impl ResolvedParameterType {
    pub fn is_handle(&self) -> bool {
        matches!(self, Self::Handle(_))
    }

    pub fn handle_type(&self) -> Option<&crate::parser::ResourceTypePath> {
        match self {
            Self::Handle(path) => Some(path),
            Self::Value(_) => None,
        }
    }

    pub fn as_type_expr(&self) -> TypeExpr {
        match self {
            Self::Handle(path) => TypeExpr::Ref(path.clone()),
            Self::Value(value) => value.clone(),
        }
    }
}

pub(crate) fn check_reference_for_handle_sink<B: BindingAddress>(
    bindings: &crate::binding_index::BindingIndex<'_, B>,
    reference: crate::resource::ReferencePathRef<'_, B>,
    expected: &ResourceTypePath,
) -> Result<bool, HandleSinkError> {
    let actual = bindings.ref_type(reference);
    if !matches!(actual, RefType::Typed(_)) && bindings.references_resource_attribute(reference) {
        return Err(HandleSinkError::value_for_handle(
            expected,
            "resource attribute value",
        ));
    }
    check_ref_type_for_handle_sink(actual, expected)
}

/// Check a value-shaped operand at a handle boundary. Only references whose
/// classified type is a structural handle may defer to structural resolution.
pub(crate) fn check_value_for_handle_sink<
    P: crate::resource::PlaceholderKind,
    B: BindingAddress,
>(
    value: &crate::resource::Value<P, B>,
    expected: &ResourceTypePath,
    reference_check: Option<Result<bool, HandleSinkError>>,
    actual: impl FnOnce() -> String,
) -> Result<bool, HandleSinkError> {
    use crate::resource::{DeferredValue, Value};
    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. } | DeferredValue::BindingRef { .. }) => {
            reference_check.unwrap_or(Ok(false))
        }
        Value::Deferred(DeferredValue::Unknown(_) | DeferredValue::Placeholder(_)) => Ok(false),
        _ => Err(HandleSinkError::value_for_handle(expected, actual())),
    }
}

/// Canonical failure produced when a classified structural argument crosses a
/// parameter boundary. Callers add only their function/module parameter
/// context; the handle-kind and resource-type decision lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleSinkError {
    message: String,
    bare_value_sink: bool,
}

impl HandleSinkError {
    pub fn is_bare_value_sink(&self) -> bool {
        self.bare_value_sink
    }

    fn value_for_handle(expected: &ResourceTypePath, actual: impl std::fmt::Display) -> Self {
        Self {
            message: format!("expected resource handle {expected}; got {actual}"),
            bare_value_sink: false,
        }
    }

    fn reference_for_handle(expected: &ResourceTypePath, actual: &ResolvedRefType) -> Self {
        let expected = AttributeType::resource_handle(&expected.provider, &expected.resource_type);
        Self::type_mismatch(
            actual.mismatch_type_names(crate::schema::TypeInSchema::schemaless(&expected)),
            actual.resource_handle_type().is_some(),
        )
    }

    fn expanded_value_for_handle<P: PlaceholderKind, B: BindingAddress>(
        expected: &ResourceTypePath,
        value: &GenericValue<P, B>,
        bindings: &BindingIndex<'_, B>,
    ) -> Self {
        if let GenericValue::Deferred(crate::resource::DeferredValue::ResourceRef { path }) = value
            && let RefType::Typed(actual) = bindings.ref_type(path)
        {
            let expected =
                AttributeType::resource_handle(&expected.provider, &expected.resource_type);
            return Self::type_mismatch(
                actual.mismatch_type_names(crate::schema::TypeInSchema::schemaless(&expected)),
                false,
            );
        }
        Self::value_for_handle(expected, typecheck::value_kind_name(value))
    }

    fn type_mismatch(types: MismatchTypeNames, actual_is_handle: bool) -> Self {
        Self {
            message: format!(
                "expected resource handle {}; got {}{}",
                types.expected(),
                if actual_is_handle {
                    "resource handle "
                } else {
                    ""
                },
                types.actual(),
            ),
            bare_value_sink: false,
        }
    }
}

impl std::fmt::Display for HandleSinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HandleSinkError {}

/// Single authority for assigning a structural handle to a resolved function
/// or module parameter type.
pub(crate) fn check_handle_sink<B: crate::resource::BindingAddress>(
    handle: &crate::eval_value::UnexpandedStructuralHandle<B>,
    resolved_parameter: &ResolvedParameterType,
) -> Result<(), HandleSinkError> {
    let ResolvedParameterType::Handle(expected) = resolved_parameter else {
        return Err(HandleSinkError {
            message: handle.bare_error().to_string(),
            bare_value_sink: true,
        });
    };

    let actual_handle = match handle.resource_handle() {
        Some(actual) => actual,
        None => {
            let kind = handle.kind();
            let binding = handle
                .alias_name()
                .map(std::borrow::Cow::Borrowed)
                .unwrap_or_else(|| std::borrow::Cow::Borrowed(handle.canonical_binding_name()));
            return Err(HandleSinkError {
                message: format!(
                    "'{binding}' is {}, not a resource of type {expected}",
                    kind.description(),
                ),
                bare_value_sink: false,
            });
        }
    };
    let actual = actual_handle.resource_type();

    if actual == expected {
        Ok(())
    } else {
        Err(HandleSinkError::type_mismatch(
            MismatchTypeNames::for_resource_types(expected, actual),
            true,
        ))
    }
}

/// Check a deferred reference whose static type may itself be a structural
/// resource handle. `Ok(false)` means the binding index cannot decide yet and
/// the post-expansion boundary remains authoritative.
pub(crate) fn check_ref_type_for_handle_sink(
    actual: RefType,
    expected: &ResourceTypePath,
) -> Result<bool, HandleSinkError> {
    match actual {
        RefType::Typed(actual) => {
            if let Some(actual_handle) = actual.resource_handle_type() {
                if &actual_handle == expected {
                    Ok(true)
                } else {
                    Err(HandleSinkError::reference_for_handle(expected, &actual))
                }
            } else {
                Err(HandleSinkError::reference_for_handle(expected, &actual))
            }
        }
        RefType::Unchecked | RefType::UnknownBinding { .. } | RefType::UnknownAttribute(_) => {
            Ok(false)
        }
    }
}

/// Resolve and classify a module or function parameter type.
///
/// A resolution error is returned as [`TypeResolutionError`]; no partially
/// classified parameter type is produced. In particular,
/// [`TypeExpr::DottedUnresolved`] can name either a resource handle or a
/// provider-defined value type. Bootstrap parser paths may retain an explicitly
/// provisional binding until an enriched parse is available.
pub fn resolve_parameter_type(
    ty: &TypeExpr,
    config: &ProviderContext,
) -> Result<ResolvedParameterType, StaticTypeResolutionError> {
    match resolve_type_expr(ty, config)? {
        TypeExpr::Ref(path) => Ok(ResolvedParameterType::Handle(path)),
        TypeExpr::DottedUnresolved(path) => {
            Err(unknown_type_resolution_error(&path.to_string(), config))
        }
        resolved @ (TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::Struct { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Union(_)) => Ok(ResolvedParameterType::Value(resolved)),
    }
}

/// Resolve a parameter type with the available catalog. An unavailable dotted
/// annotation is neither a proven value type nor a proven resource handle.
/// Record it while preserving that uncertainty for later substitution.
pub fn resolve_parameter_type_for_check(
    ty: &TypeExpr,
    config: &ProviderContext,
) -> Result<Option<ResolvedParameterType>, StaticTypeResolutionError> {
    let resolved = resolve_type_expr(ty, config)?;
    if matches!(resolved, TypeExpr::DottedUnresolved(_)) {
        return Ok(None);
    }
    resolve_parameter_type(&resolved, config).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(transparent)]
pub struct TypeResolutionError {
    cause: StaticTypeResolutionError,
}

impl From<StaticTypeResolutionError> for TypeResolutionError {
    fn from(cause: StaticTypeResolutionError) -> Self {
        Self { cause }
    }
}

impl TypeResolutionError {
    fn unresolved_type_name(&self) -> &str {
        match &self.cause {
            StaticTypeResolutionError::UnknownCustomType { name }
            | StaticTypeResolutionError::UnknownCustomTypeWithSuggestion { name, .. }
            | StaticTypeResolutionError::ConflictingScalarKinds { name, .. }
            | StaticTypeResolutionError::MissingDeclaration { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeResolutionDeclarationKind {
    Argument,
    Attribute,
    Export,
    FunctionParameter,
    FunctionReturn,
}

impl std::fmt::Display for TypeResolutionDeclarationKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Argument => formatter.write_str("argument"),
            Self::Attribute => formatter.write_str("attribute"),
            Self::Export => formatter.write_str("export"),
            Self::FunctionParameter => formatter.write_str("function parameter"),
            Self::FunctionReturn => formatter.write_str("function return type"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeResolutionContext {
    Declaration {
        kind: TypeResolutionDeclarationKind,
        name: String,
    },
    Description(String),
}

impl std::fmt::Display for TypeResolutionContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Declaration { kind, name } => write!(formatter, "{kind} '{name}'"),
            Self::Description(description) => formatter.write_str(description),
        }
    }
}

/// Details are always delivered inside `Located`; location is mandatory.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::{DeclarationLocation, Located}, validation::TypeResolutionDiagnostic};
/// fn location(finding: &Located<TypeResolutionDiagnostic>) {
///     let _: Option<&DeclarationLocation> = finding.location();
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{context}: {error}")]
pub struct TypeResolutionDiagnostic {
    context: TypeResolutionContext,
    error: TypeResolutionError,
}

impl TypeResolutionDiagnostic {
    fn declaration(
        kind: TypeResolutionDeclarationKind,
        name: String,
        error: TypeResolutionError,
        location: crate::parser::DeclarationLocation,
    ) -> crate::parser::Located<Self> {
        crate::parser::Located::new(
            location,
            Self {
                context: TypeResolutionContext::Declaration { kind, name },
                error,
            },
        )
    }
    fn description(
        description: String,
        error: TypeResolutionError,
        location: crate::parser::DeclarationLocation,
    ) -> crate::parser::Located<Self> {
        crate::parser::Located::new(
            location,
            Self {
                context: TypeResolutionContext::Description(description),
                error,
            },
        )
    }
}

impl crate::parser::Located<TypeResolutionDiagnostic> {
    fn covers_parser_error(
        &self,
        source_file: &std::path::Path,
        parse_error: &crate::parser::ParseError,
    ) -> bool {
        let crate::parser::ParseError::UnresolvedDeclarationType { token, .. } = parse_error else {
            return false;
        };
        self.location().file().path() == source_file
            && self.location().span().contains(token.span())
            && self.detail().error.unresolved_type_name() == token.name()
    }
}

/// A parser backstop that may describe the same unresolved declaration as a
/// contextual type-resolution diagnostic.
#[derive(Debug, Clone, Copy)]
pub struct TypeResolutionParserError<'a> {
    source_file: &'a std::path::Path,
    error: &'a crate::parser::ParseError,
}

impl<'a> TypeResolutionParserError<'a> {
    pub fn new(source_file: &'a std::path::Path, error: &'a crate::parser::ParseError) -> Self {
        Self { source_file, error }
    }
}

/// The sole decision point for replacing a parser backstop with typed
/// declaration diagnostics. Both resolver/CLI and LSP paths use this gate.
pub(crate) fn type_resolution_parser_error_is_covered<'a>(
    diagnostics: impl IntoIterator<Item = &'a crate::parser::Located<TypeResolutionDiagnostic>>,
    parser_error: TypeResolutionParserError<'_>,
) -> bool {
    diagnostics.into_iter().any(|diagnostic| {
        diagnostic.covers_parser_error(parser_error.source_file, parser_error.error)
    })
}

/// Core-owned declaration-resolution outcome consumed by CLI and LSP.
///
/// `covered_parser_error` lets a frontend omit a coarse parser backstop only
/// when one of these typed diagnostics owns the same file, type token, and
/// source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTypeResolutionReport {
    diagnostics: Vec<crate::parser::Located<TypeResolutionDiagnostic>>,
    covered_parser_error: bool,
    resolved_argument_types: ResolvedArgumentTypes,
}

/// Per-argument outcomes minted by declaration resolution. An unresolved
/// declaration cannot disable checks on unrelated dependency operands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArgumentTypes {
    types: IndexMap<String, DependencyArgumentResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DependencyArgumentResolution {
    Resolved(ResolvedDependencyArgumentType),
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedDependencyArgumentType {
    ManagedHandle,
    Value(String),
}

impl FileTypeResolutionReport {
    pub fn resolved_argument_types(&self) -> &ResolvedArgumentTypes {
        &self.resolved_argument_types
    }

    pub fn diagnostics(&self) -> &[crate::parser::Located<TypeResolutionDiagnostic>] {
        &self.diagnostics
    }

    pub fn into_diagnostics(self) -> Vec<crate::parser::Located<TypeResolutionDiagnostic>> {
        self.diagnostics
    }

    pub fn covered_parser_error(&self) -> bool {
        self.covered_parser_error
    }
}

fn resolve_declaration_type(
    ty: &mut TypeExpr,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    site: &deferred_types::ProviderTypeCheckSite,
) -> Vec<TypeResolutionError> {
    match ty {
        TypeExpr::List(inner) | TypeExpr::Map(inner) => {
            resolve_declaration_type(inner, config, site)
        }
        TypeExpr::Union(members) => members
            .iter_mut()
            .flat_map(|member| resolve_declaration_type(member, config, site))
            .collect(),
        TypeExpr::Struct { fields } => fields
            .iter_mut()
            .flat_map(|(_, field)| resolve_declaration_type(field, config, site))
            .collect(),
        _ => match resolve_type_expr(ty, config.catalog()) {
            Ok(resolved) => {
                config.declaration(&resolved, site.clone());
                if matches!(
                    resolved,
                    TypeExpr::UnloadedSchemaType(_) | TypeExpr::DottedUnresolved(_)
                ) {
                    config.record(&resolved, site.clone());
                }
                *ty = resolved;
                Vec::new()
            }
            Err(error) => vec![error.into()],
        },
    }
}

fn resolve_file_type_exprs_impl<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &mut crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<
    (
        Vec<crate::parser::Located<TypeResolutionDiagnostic>>,
        ResolvedArgumentTypes,
    ),
    crate::parser::AnalysisTreeError,
> {
    let annotations = crate::parser::AnalysisTree::try_from_file(parsed)?
        .type_annotations()
        .to_vec();
    Ok(resolve_file_type_exprs_with_annotations(
        parsed,
        config,
        &annotations,
    ))
}

fn resolve_file_type_exprs_with_annotations<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &mut crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    annotations: &[crate::parser::Located<TypeResolutionContext>],
) -> (
    Vec<crate::parser::Located<TypeResolutionDiagnostic>>,
    ResolvedArgumentTypes,
) {
    use deferred_types::ProviderTypeCheckSite;
    let mut occurrences = std::collections::HashMap::<TypeResolutionContext, usize>::new();
    let mut annotation = |kind, name: &str| {
        let context = TypeResolutionContext::Declaration {
            kind,
            name: name.to_string(),
        };
        let occurrence = occurrences.entry(context.clone()).or_default();
        let location = annotations
            .iter()
            .filter(|annotation| annotation.detail() == &context)
            .nth(*occurrence)
            .expect("type annotation validated at the analysis boundary")
            .location()
            .clone();
        *occurrence += 1;
        location
    };
    let mut errors = Vec::new();
    let mut argument_types = IndexMap::new();
    for argument in &mut parsed.arguments {
        let location = annotation(TypeResolutionDeclarationKind::Argument, &argument.name);
        let context = format!("argument '{}'", argument.name);
        let site = ProviderTypeCheckSite::new(location.clone(), context);
        let failures = resolve_declaration_type(&mut argument.static_type, config, &site);
        let resolution = if failures.is_empty() {
            DependencyArgumentResolution::Resolved(match &argument.static_type {
                TypeExpr::Ref(_) => ResolvedDependencyArgumentType::ManagedHandle,
                value => ResolvedDependencyArgumentType::Value(value.to_string()),
            })
        } else {
            DependencyArgumentResolution::Unresolved
        };
        argument_types.insert(argument.name.clone(), resolution);
        for error in failures {
            errors.push(TypeResolutionDiagnostic::declaration(
                TypeResolutionDeclarationKind::Argument,
                argument.name.clone(),
                error,
                location.clone(),
            ));
        }
    }
    for attribute in &mut parsed.attribute_params {
        // Declaration and runtime checks share the parser-owned annotation
        // token, so deduplication never reconstructs a site from coordinates.
        if let Some(ty) = &mut attribute.static_type {
            let location = annotation(TypeResolutionDeclarationKind::Attribute, &attribute.name);
            let site = ProviderTypeCheckSite::new(
                location.clone(),
                format!("attribute '{}'", attribute.name),
            );
            for error in resolve_declaration_type(ty, config, &site) {
                errors.push(TypeResolutionDiagnostic::declaration(
                    TypeResolutionDeclarationKind::Attribute,
                    attribute.name.clone(),
                    error,
                    location.clone(),
                ));
            }
        }
    }
    for export in &mut parsed.export_params {
        let name = export.name().to_string();
        if let Some(ty) = export.type_expr_opt_mut() {
            let location = annotation(TypeResolutionDeclarationKind::Export, &name);
            let site = ProviderTypeCheckSite::new(location.clone(), format!("export '{name}'"));
            for error in resolve_declaration_type(ty, config, &site) {
                errors.push(TypeResolutionDiagnostic::declaration(
                    TypeResolutionDeclarationKind::Export,
                    name.clone(),
                    error,
                    location.clone(),
                ));
            }
        }
    }
    for function in parsed.user_functions.values_mut() {
        let previous_errors = errors.len();
        for parameter in &mut function.params {
            if let Some(ty) = &mut parameter.param_type {
                let name = format!("{}' parameter '{}", function.name, parameter.name);
                let location = annotation(TypeResolutionDeclarationKind::FunctionParameter, &name);
                let site = ProviderTypeCheckSite::new(
                    location.clone(),
                    format!(
                        "function '{}' parameter '{}'",
                        function.name, parameter.name
                    ),
                );
                for error in resolve_declaration_type(ty, config, &site) {
                    errors.push(TypeResolutionDiagnostic::declaration(
                        TypeResolutionDeclarationKind::FunctionParameter,
                        name.clone(),
                        error,
                        location.clone(),
                    ));
                }
            }
        }
        if let Some(ty) = &mut function.return_type {
            let location = annotation(
                TypeResolutionDeclarationKind::FunctionReturn,
                &function.name,
            );
            let site = ProviderTypeCheckSite::new(
                location.clone(),
                format!("function '{}' return value", function.name),
            );
            for error in resolve_declaration_type(ty, config, &site) {
                errors.push(TypeResolutionDiagnostic::declaration(
                    TypeResolutionDeclarationKind::FunctionReturn,
                    function.name.clone(),
                    error,
                    location.clone(),
                ));
            }
        }
        if errors.len() != previous_errors {
            // This is the function-check session's semantic key, shared with
            // function_declaration_failed. Rendering still uses the mandatory
            // parser-owned annotation locations above.
            let source = function
                .annotation_source_file
                .as_deref()
                .or(function.body.source_file.as_deref())
                .unwrap_or_else(|| std::path::Path::new("<config>"));
            config.fail_function_declaration(source, &function.name);
        }
    }
    (
        errors,
        ResolvedArgumentTypes {
            types: argument_types,
        },
    )
}

/// Resolve a signature snapshot when the consumer's provider catalog may be
/// incomplete. The snapshot does not own diagnostics, so a resolution failure
/// retains its typed unchecked reason for the later authoritative module load.
pub fn resolve_consumer_static_type_for_signature(
    type_expr: &TypeExpr,
    config: &ProviderContext,
) -> ConsumerStaticType {
    resolve_consumer_static_type(type_expr, config)
}

/// Resolve every module-boundary type declaration in place. Diagnostics
/// retain the declaration kind/name and authored source when available, giving
/// every frontend the same contextual error surface.
pub fn resolve_file_type_exprs<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &mut crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Vec<crate::parser::Located<TypeResolutionDiagnostic>>, crate::parser::AnalysisTreeError>
{
    Ok(resolve_file_type_exprs_report(parsed, config, None)?.into_diagnostics())
}

/// Resolve declarations and decide in core whether a typed finding supersedes
/// an optional parser backstop for the same authored type token.
///
/// Boundary failures cannot be mistaken for a successful empty report:
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, validation::{FileTypeResolutionReport, resolve_file_type_exprs_report, deferred_types::ProviderTypeCheckContext}};
/// fn analyze(file: &mut ParsedFile, context: &dyn ProviderTypeCheckContext) -> FileTypeResolutionReport {
///     resolve_file_type_exprs_report(file, context, None)
/// }
/// ```
pub fn resolve_file_type_exprs_report<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &mut crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    parser_error: Option<TypeResolutionParserError<'_>>,
) -> Result<FileTypeResolutionReport, crate::parser::AnalysisTreeError> {
    let (diagnostics, resolved_argument_types) = resolve_file_type_exprs_impl(parsed, config)?;
    let covered_parser_error = parser_error.is_some_and(|parser_error| {
        type_resolution_parser_error_is_covered(&diagnostics, parser_error)
    });
    Ok(FileTypeResolutionReport {
        diagnostics,
        covered_parser_error,
        resolved_argument_types,
    })
}

/// Resolve a copy of a sealed input using its already validated annotations.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, validation::{resolve_file_type_exprs_from_analysis, deferred_types::ProviderTypeCheckContext}};
/// fn resolve(file: &ParsedFile, session: &dyn ProviderTypeCheckContext) {
///     resolve_file_type_exprs_from_analysis(file, session, None);
/// }
/// ```
pub fn resolve_file_type_exprs_from_analysis<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B> + Clone,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B> + Clone,
>(
    tree: &crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B, E>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    parser_error: Option<TypeResolutionParserError<'_>>,
) -> (
    crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    FileTypeResolutionReport,
) {
    let mut parsed = tree.file().clone();
    let (diagnostics, resolved_argument_types) =
        resolve_file_type_exprs_with_annotations(&mut parsed, config, tree.type_annotations());
    let covered_parser_error = parser_error
        .is_some_and(|error| type_resolution_parser_error_is_covered(&diagnostics, error));
    (
        parsed,
        FileTypeResolutionReport {
            diagnostics,
            resolved_argument_types,
            covered_parser_error,
        },
    )
}

/// Resolve declarations with an intentionally incomplete provider context.
///
/// Upstream export discovery runs in the consumer process, whose providers
/// need not include every provider used by the upstream project. A type that
/// cannot be named in that context carries no sound consumer-side constraint.
///
/// Unnameable declarations retain both their authored type and the typed
/// resolution failure instead of remaining ambiguously unresolved.
pub(crate) fn resolve_consumer_static_type(
    type_expr: &TypeExpr,
    config: &ProviderContext,
) -> ConsumerStaticType {
    match resolve_type_expr(type_expr, config) {
        Ok(resolved) => match unresolved_dotted_annotation(&resolved) {
            Some(path) => StaticType::Unchecked(UnnameableInConsumer::new(
                type_expr.clone(),
                unknown_type_resolution_error(&path.to_string(), config),
            )),
            None => StaticType::Known(ConsumerResolvedType::new(resolved)),
        },
        Err(resolution_error) => StaticType::Unchecked(UnnameableInConsumer::new(
            type_expr.clone(),
            resolution_error,
        )),
    }
}

/// An opaque consumer-resolved type cannot contain an unclassified dotted leaf.
fn unresolved_dotted_annotation(ty: &TypeExpr) -> Option<&crate::parser::ResourceTypePath> {
    match ty {
        TypeExpr::DottedUnresolved(path) => Some(path),
        TypeExpr::List(inner) | TypeExpr::Map(inner) => unresolved_dotted_annotation(inner),
        TypeExpr::Union(members) => members.iter().find_map(unresolved_dotted_annotation),
        TypeExpr::Struct { fields } => fields
            .iter()
            .find_map(|(_, field)| unresolved_dotted_annotation(field)),
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::Ref(_)
        | TypeExpr::StringLiteral(_) => None,
    }
}

pub fn resolve_attribute_static_types_for_consumer(
    parameters: &[crate::parser::AttributeParameter],
    config: &ProviderContext,
) -> Vec<crate::parser::ConsumerAttributeParameter> {
    parameters
        .iter()
        .cloned()
        .map(|parameter| {
            parameter.map_type_expr(|type_expr| resolve_consumer_static_type(&type_expr, config))
        })
        .collect()
}

/// Canonicalize every declaration that the consumer can name while retaining
/// authored syntax for declarations outside its provider catalog.
pub fn resolve_file_type_exprs_best_effort<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &mut crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), crate::parser::AnalysisTreeError> {
    resolve_file_type_exprs_impl(parsed, config)?;
    Ok(())
}

pub fn resolve_export_static_type_for_consumer(
    static_type: ExportStaticType,
    config: &ProviderContext,
) -> ConsumerExportStaticType {
    match static_type {
        ExportStaticType::Declared(type_expr) => {
            ConsumerExportStaticType::Declared(resolve_consumer_static_type(&type_expr, config))
        }
        ExportStaticType::Inferred(StaticType::Known(type_expr)) => {
            let resolved = match resolve_consumer_static_type(&type_expr, config) {
                StaticType::Known(resolved) => StaticType::Known(resolved),
                StaticType::Unchecked(reason) => StaticType::Unchecked(
                    ConsumerInferredUncheckedReason::UnnameableInConsumer(reason),
                ),
            };
            ConsumerExportStaticType::Inferred(resolved)
        }
        ExportStaticType::Inferred(StaticType::Unchecked(reason)) => {
            ConsumerExportStaticType::Inferred(StaticType::Unchecked(reason.into()))
        }
    }
}

/// Return whether a merged configuration directory is a module definition.
///
/// This is the classification used by the root-owned-block validators: a
/// top-level `arguments` or `attributes` block marks the directory as a
/// module whose provider configuration comes from its caller.
#[must_use]
pub fn is_module_definition<
    E,
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    D,
    B: BindingAddress,
>(
    parsed: &crate::parser::File<E, P, T, D, B>,
) -> bool {
    !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty()
}

/// Report root-only blocks in modules and argument blocks in root configurations.
pub fn configuration_placement_diagnostics<
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
>(
    tree: &crate::parser::AnalysisTree<'_, P, T, D, B>,
) -> Vec<crate::parser::Located<&'static str>> {
    use crate::parser::ConfigurationBlock;
    tree.syntax().configuration_blocks().iter().filter_map(|block| {
        let message = match block.detail() {
            ConfigurationBlock::Arguments if tree.is_root_configuration() => "arguments blocks are only valid inside module definitions, not in root configurations.",
            ConfigurationBlock::Provider if tree.is_module_definition() => "provider blocks are not allowed inside modules. Define providers at the root configuration level.",
            ConfigurationBlock::State if tree.is_module_definition() => "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level.",
            ConfigurationBlock::Backend if tree.is_module_definition() => "backend blocks are not allowed inside modules. Define the backend at the root configuration level.",
            ConfigurationBlock::UpstreamState if tree.is_module_definition() => "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level.",
            ConfigurationBlock::Exports if tree.is_module_definition() => "exports blocks are not allowed inside modules. Define exports at the root configuration level.",
            _ => return None,
        };
        Some(crate::parser::Located::new(block.location().clone(), message))
    }).collect()
}

/// Check that a module file does not contain provider blocks.
///
/// Provider configuration should only be defined at the root configuration level,
/// not inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_provider_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    if is_module_definition(parsed) && !parsed.providers.is_empty() {
        return Err(
            "provider blocks are not allowed inside modules. Define providers at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain state blocks.
///
/// State changes should only be defined at the root configuration level, not
/// inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_state_blocks_in_module<E>(
    parsed: &crate::parser::File<E>,
) -> Result<(), String> {
    if is_module_definition(parsed) && !parsed.state_blocks.is_empty() {
        return Err(
            "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain a backend block.
///
/// Backend configuration should only be defined at the root configuration
/// level, not inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_backend_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    if is_module_definition(parsed) && parsed.backend.is_some() {
        return Err(
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain upstream state declarations.
///
/// Upstream state dependencies should only be defined at the root
/// configuration level, not inside modules (files with `arguments` or
/// `attributes` blocks).
pub fn validate_no_upstream_states_in_module<E>(
    parsed: &crate::parser::File<E>,
) -> Result<(), String> {
    if is_module_definition(parsed) && !parsed.upstream_states.is_empty() {
        return Err(
            "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain an exports block.
///
/// State exports should only be defined at the root configuration level, not
/// inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_exports_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    if is_module_definition(parsed) && !parsed.export_params.is_empty() {
        return Err(
            "exports blocks are not allowed inside modules. Define exports at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Stable semantic category used for source-aware deduplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleCallFindingKind {
    UnknownArgument,
    MissingRequiredArgument,
    ValueType,
    HandleSink,
    BindingUse,
    ReferenceType,
    TypeResolution,
    Constraint,
}

/// Parser-authored location retained without converting it to frontend
/// coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleCallFindingAnchor {
    location: crate::parser::DeclarationLocation,
    pub source_file: SourceFileId,
    pub span: SourceSpan,
}

impl ModuleCallFindingAnchor {
    fn new(location: crate::parser::DeclarationLocation) -> Self {
        Self {
            source_file: location.file().clone(),
            span: location.span(),
            location,
        }
    }
    pub fn location(&self) -> &crate::parser::DeclarationLocation {
        &self.location
    }
}

/// Authored source identity that owns a module-graph diagnostic.
///
/// A reachable graph can be owned by a concrete module call or by an uncalled
/// `use` declaration. Keeping those cases distinct prevents editor routing
/// from inventing a module-call identity for an import-only path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleCallFindingOwnerSource {
    ModuleCall(ModuleCallId),
    Authored(ModuleCallFindingAnchor),
}

impl ModuleCallFindingOwnerSource {
    pub fn source_file(&self) -> &SourceFileId {
        match self {
            Self::ModuleCall(call) => call.source_file_id(),
            Self::Authored(anchor) => &anchor.source_file,
        }
    }

    pub fn location(&self) -> &crate::parser::DeclarationLocation {
        match self {
            Self::ModuleCall(call) => call.declaration_location(),
            Self::Authored(anchor) => anchor.location(),
        }
    }

    pub fn span(&self) -> SourceSpan {
        match self {
            Self::ModuleCall(call) => call.call_span(),
            Self::Authored(anchor) => anchor.span,
        }
    }

    pub fn module_call(&self) -> Option<&ModuleCallId> {
        match self {
            Self::ModuleCall(call) => Some(call),
            Self::Authored(_anchor) => None,
        }
    }
}

/// The outermost authored owner and the immediate source whose boundary
/// failed. Either position may be a call or an authored declaration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleCallFindingOwner {
    pub root: ModuleCallFindingOwnerSource,
    pub immediate: ModuleCallFindingOwnerSource,
}

/// Typed reference-boundary diagnostic shared by authored and expanded calls.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleCallReferenceDiagnostic {
    call_label: String,
    identity: ModuleCallReferenceIdentity,
    display_path: ModuleCallDiagnosticPath,
    declared_constraint: bool,
}

impl std::fmt::Display for ModuleCallReferenceDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep the established declaration-constraint wording for literal
        // sets and unions. This affects rendering only: the typed reference
        // identity still covers the resolver's assignment backstop.
        if self.declared_constraint
            && let ModuleCallRefErrorKind::TypeMismatch { types, .. } = &self.identity.kind
        {
            return write!(
                formatter,
                "module call '{}': argument '{}': Type mismatch: {types} (from {})",
                self.call_label,
                self.identity.argument,
                self.display_path.to_dot_string(),
            );
        }
        self.display_path.format_error(
            formatter,
            &self.call_label,
            &self.identity.argument,
            &self.identity.kind,
        )
    }
}

impl std::error::Error for ModuleCallReferenceDiagnostic {}

/// The sole message-rendering authority for module-call findings.
#[derive(Debug, Clone, PartialEq)]
pub enum ModuleCallFindingMessage {
    UnknownArgument {
        module: String,
        argument: String,
        suggestion: Option<String>,
    },
    MissingRequiredArgument {
        module: String,
        argument: String,
    },
    ValueType {
        call_label: String,
        argument: String,
        error: Box<typecheck::TypeExprValueError>,
    },
    HandleSink {
        call_label: String,
        argument: String,
        error: HandleSinkError,
    },
    BindingUse {
        argument: String,
        error: Box<BindingResolutionError>,
    },
    NestedResourceBindingUse(Box<BindingUseFinding>),
    ReferenceType(Box<ModuleCallReferenceDiagnostic>),
    TypeResolution(Box<crate::parser::Located<TypeResolutionDiagnostic>>),
    AttributeReference(Box<CompositionAttributeRefError>),
    Constraint(crate::module_resolver::ConstraintDiagnostic),
}

impl ModuleCallFindingMessage {
    #[cfg(test)]
    fn type_resolution_for_test(context: String, error: StaticTypeResolutionError) -> Self {
        Self::TypeResolution(Box::new(TypeResolutionDiagnostic::description(
            context,
            error.into(),
            crate::parser::parse_file(
                "let diagnostic = 1",
                std::path::Path::new("<test>"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap()
            .parser_binding_metadata
            .syntax()
            .declarations()[0]
                .location()
                .clone(),
        )))
    }

    #[cfg(test)]
    fn attribute_reference_for_test(error: CompositionAttributeRefError) -> Self {
        Self::AttributeReference(Box::new(error))
    }

    /// Schema enum detail retained for editor code actions without changing
    /// this message's canonical rendering or diagnostic ownership.
    pub fn type_error(&self) -> Option<&TypeError> {
        match self {
            Self::BindingUse {
                argument: _argument,
                error,
            } => error.type_error(),
            Self::NestedResourceBindingUse(finding) => finding.error.type_error(),
            Self::UnknownArgument {
                module: _module,
                argument: _argument,
                suggestion: _suggestion,
            } => None,
            Self::MissingRequiredArgument {
                module: _module,
                argument: _argument,
            } => None,
            Self::ValueType {
                call_label: _call_label,
                argument: _argument,
                error: _error,
            } => None,
            Self::HandleSink {
                call_label: _call_label,
                argument: _argument,
                error: _error,
            } => None,
            Self::ReferenceType(_diagnostic) => None,
            Self::TypeResolution(_error) => None,
            Self::AttributeReference(_error) => None,
            Self::Constraint(_diagnostic) => None,
        }
    }

    pub(crate) fn covers_missing_argument(&self, module: &str, argument: &str) -> bool {
        match self {
            Self::MissingRequiredArgument {
                module: finding_module,
                argument: finding_argument,
            } => finding_module == module && finding_argument == argument,
            Self::BindingUse {
                argument: finding_argument,
                error: _,
            } => finding_argument == argument,
            Self::UnknownArgument {
                module: _,
                argument: _,
                suggestion: _,
            }
            | Self::ValueType {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::HandleSink {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::NestedResourceBindingUse(_)
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => false,
        }
    }

    pub(crate) fn covers_invalid_argument_type(
        &self,
        argument: &str,
        mismatch: &typecheck::TypeExprValueError,
    ) -> bool {
        match self {
            Self::ValueType {
                call_label: _,
                argument: finding_argument,
                error,
            } => finding_argument == argument && error.as_ref() == mismatch,
            Self::BindingUse {
                argument: finding_argument,
                error: _,
            } => finding_argument == argument,
            Self::UnknownArgument {
                module: _,
                argument: _,
                suggestion: _,
            }
            | Self::MissingRequiredArgument {
                module: _,
                argument: _,
            }
            | Self::HandleSink {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::NestedResourceBindingUse(_)
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => false,
        }
    }

    pub(crate) fn covers_bare_handle_argument(&self, argument: &str) -> bool {
        match self {
            Self::HandleSink {
                call_label: _,
                argument: finding_argument,
                error: _,
            }
            | Self::BindingUse {
                argument: finding_argument,
                error: _,
            } => finding_argument == argument,
            Self::UnknownArgument {
                module: _,
                argument: _,
                suggestion: _,
            }
            | Self::MissingRequiredArgument {
                module: _,
                argument: _,
            }
            | Self::ValueType {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::NestedResourceBindingUse(_)
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => false,
        }
    }

    pub(crate) fn covers_unknown_argument(&self, module: &str, argument: &str) -> bool {
        match self {
            Self::UnknownArgument {
                module: finding_module,
                argument: finding_argument,
                suggestion: _,
            } => finding_module == module && finding_argument == argument,
            Self::MissingRequiredArgument {
                module: _,
                argument: _,
            }
            | Self::ValueType {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::HandleSink {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::BindingUse {
                argument: _,
                error: _,
            }
            | Self::NestedResourceBindingUse(_)
            | Self::ReferenceType(_)
            | Self::TypeResolution(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => false,
        }
    }

    pub(crate) fn covers_reference_type(&self, error: &ModuleCallRefError) -> bool {
        match self {
            Self::ReferenceType(diagnostic) => diagnostic.identity.covers(&error.identity),
            Self::UnknownArgument {
                module: _,
                argument: _,
                suggestion: _,
            }
            | Self::MissingRequiredArgument {
                module: _,
                argument: _,
            }
            | Self::ValueType {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::HandleSink {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::BindingUse {
                argument: _,
                error: _,
            }
            | Self::NestedResourceBindingUse(_)
            | Self::TypeResolution(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => false,
        }
    }

    pub(crate) fn type_resolution_diagnostic(
        &self,
    ) -> Option<&crate::parser::Located<TypeResolutionDiagnostic>> {
        match self {
            Self::TypeResolution(diagnostic) => Some(diagnostic),
            Self::UnknownArgument {
                module: _,
                argument: _,
                suggestion: _,
            }
            | Self::MissingRequiredArgument {
                module: _,
                argument: _,
            }
            | Self::ValueType {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::HandleSink {
                call_label: _,
                argument: _,
                error: _,
            }
            | Self::BindingUse {
                argument: _,
                error: _,
            }
            | Self::NestedResourceBindingUse(_)
            | Self::ReferenceType(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => None,
        }
    }
}

impl std::fmt::Display for ModuleCallFindingMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownArgument {
                module,
                argument,
                suggestion,
            } => {
                write!(
                    formatter,
                    "Unknown argument '{argument}' for module '{module}'"
                )?;
                if let Some(suggestion) = suggestion {
                    write!(formatter, ". Did you mean '{suggestion}'?")?;
                }
                Ok(())
            }
            Self::MissingRequiredArgument { module, argument } => write!(
                formatter,
                "Missing required argument '{argument}' for module '{module}'"
            ),
            Self::ValueType {
                call_label,
                argument,
                error,
            } => write!(
                formatter,
                "module call '{call_label}': argument '{argument}': {error}"
            ),
            Self::HandleSink {
                call_label,
                argument,
                error,
            } => write!(
                formatter,
                "module call '{call_label}': argument '{argument}': {error}"
            ),
            Self::BindingUse {
                argument: _argument,
                error,
            } => error.fmt(formatter),
            Self::NestedResourceBindingUse(finding) => write!(
                formatter,
                "{}: {} at line {}",
                crate::utils::normalize_source_path(finding.use_site.source_file.path()).display(),
                finding.error,
                finding.use_site.span.start_line(),
            ),
            Self::ReferenceType(error) => error.fmt(formatter),
            Self::TypeResolution(error) => error.detail().fmt(formatter),
            Self::AttributeReference(error) => error.fmt(formatter),
            Self::Constraint(diagnostic) => diagnostic.fmt(formatter),
        }
    }
}

impl std::error::Error for ModuleCallFindingMessage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TypeResolution(error) => Some(error.detail()),
            Self::AttributeReference(error) => Some(error.as_ref()),
            Self::UnknownArgument {
                module: _module,
                argument: _argument,
                suggestion: _suggestion,
            } => None,
            Self::MissingRequiredArgument {
                module: _module,
                argument: _argument,
            } => None,
            Self::ValueType {
                call_label: _call_label,
                argument: _argument,
                error: _error,
            } => None,
            Self::HandleSink {
                call_label: _call_label,
                argument: _argument,
                error: _error,
            } => None,
            Self::BindingUse {
                argument: _argument,
                error: _error,
            } => None,
            Self::NestedResourceBindingUse(finding) => Some(finding.as_ref()),
            Self::ReferenceType(_error) => None,
            Self::Constraint(_diagnostic) => None,
        }
    }
}

/// Typed identity used to merge equivalent findings across authored and
/// expanded validation passes.
#[derive(Debug, Clone, PartialEq)]
enum ModuleCallFindingIdentity {
    BindingUse {
        call: ModuleCallId,
        argument: String,
        error: BindingResolutionError,
    },
    ReferenceType(ModuleCallReferenceIdentity),
}

/// One module-call result with typed ownership and source.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleCallFinding {
    kind: ModuleCallFindingKind,
    owners: Vec<ModuleCallFindingOwner>,
    anchor: ModuleCallFindingAnchor,
    message: ModuleCallFindingMessage,
    identity: Option<ModuleCallFindingIdentity>,
}

impl ModuleCallFinding {
    fn argument_value(
        kind: ModuleCallFindingKind,
        owner: ModuleCallFindingOwner,
        call: &ModuleCallId,
        message: ModuleCallFindingMessage,
        identity: Option<ModuleCallFindingIdentity>,
    ) -> Self {
        Self {
            kind,
            owners: vec![owner],
            anchor: ModuleCallFindingAnchor::new(
                match &message {
                    ModuleCallFindingMessage::ValueType { argument, .. }
                    | ModuleCallFindingMessage::HandleSink { argument, .. }
                    | ModuleCallFindingMessage::BindingUse { argument, .. } => call
                        .argument_value_location(argument)
                        .unwrap_or_else(|| call.declaration_location()),
                    ModuleCallFindingMessage::ReferenceType(error) => call
                        .argument_value_location(&error.identity.argument)
                        .unwrap_or_else(|| call.declaration_location()),
                    ModuleCallFindingMessage::Constraint(
                        crate::module_resolver::ConstraintDiagnostic::Directory(_),
                    ) => unreachable!("directory findings have authored anchors"),
                    ModuleCallFindingMessage::Constraint(
                        crate::module_resolver::ConstraintDiagnostic::ModuleCall(diagnostic),
                    ) => match &diagnostic.kind {
                        crate::module_resolver::ModuleConstraintKind::ArgumentValidation {
                            argument,
                        } => call
                            .argument_value_location(argument)
                            .unwrap_or_else(|| call.declaration_location()),
                        crate::module_resolver::ModuleConstraintKind::Require => {
                            call.declaration_location()
                        }
                    },
                    _ => call.declaration_location(),
                }
                .clone(),
            ),
            message,
            identity,
        }
    }

    pub fn from_type_resolution(
        diagnostic: crate::parser::Located<TypeResolutionDiagnostic>,
    ) -> Self {
        let location = diagnostic.location();
        let anchor = ModuleCallFindingAnchor::new(location.clone());
        let owner = ModuleCallFindingOwnerSource::Authored(anchor.clone());
        Self {
            kind: ModuleCallFindingKind::TypeResolution,
            owners: vec![ModuleCallFindingOwner {
                root: owner.clone(),
                immediate: owner,
            }],
            anchor,
            message: ModuleCallFindingMessage::TypeResolution(Box::new(diagnostic)),
            identity: None,
        }
    }

    fn reference_type(
        call_label: &str,
        identity: ModuleCallReferenceIdentity,
        owner: ModuleCallFindingOwner,
    ) -> Self {
        let display_path = identity.path.clone();
        Self::reference_type_with_display_path(call_label, identity, display_path, owner, false)
    }

    fn reference_type_with_display_path(
        call_label: &str,
        identity: ModuleCallReferenceIdentity,
        display_path: ModuleCallDiagnosticPath,
        owner: ModuleCallFindingOwner,
        declared_constraint: bool,
    ) -> Self {
        let call = identity.call.clone();
        Self::argument_value(
            ModuleCallFindingKind::ReferenceType,
            owner,
            &call,
            ModuleCallFindingMessage::ReferenceType(Box::new(ModuleCallReferenceDiagnostic {
                call_label: call_label.to_string(),
                identity: identity.clone(),
                display_path,
                declared_constraint,
            })),
            Some(ModuleCallFindingIdentity::ReferenceType(identity)),
        )
    }

    fn binding_use(
        call_label: &str,
        call: &ModuleCallId,
        argument: &str,
        finding: &BindingUseFinding,
        owner: ModuleCallFindingOwner,
    ) -> Self {
        let (kind, message) = match &finding.error {
            BindingResolutionError::ValueType(error) => (
                ModuleCallFindingKind::ValueType,
                ModuleCallFindingMessage::ValueType {
                    call_label: call_label.to_string(),
                    argument: argument.to_string(),
                    error: error.clone(),
                },
            ),
            BindingResolutionError::ReferenceType(error) => {
                return Self::reference_type_with_display_path(
                    call_label,
                    ModuleCallReferenceIdentity {
                        call: call.clone(),
                        argument: argument.to_string(),
                        path: error.path.clone(),
                        reference_position: None,
                        binding_use: Some(BindingUseReferenceIdentity {
                            source_file: finding.use_site.source_file.clone(),
                            span: finding.use_site.span,
                        }),
                        kind: ModuleCallRefErrorKind::TypeMismatch {
                            types: error.types.clone(),
                            declared: error.declared.clone(),
                        },
                    },
                    error.path.clone(),
                    owner,
                    error.declared_constraint,
                );
            }
            error @ (BindingResolutionError::Type(_)
            | BindingResolutionError::DeferredAccess(_)
            | BindingResolutionError::CustomType(_)
            | BindingResolutionError::TypeResolution(_)
            | BindingResolutionError::Existing(_)) => (
                ModuleCallFindingKind::BindingUse,
                ModuleCallFindingMessage::BindingUse {
                    argument: argument.to_string(),
                    error: Box::new(error.clone()),
                },
            ),
        };
        Self::argument_value(
            kind,
            owner,
            call,
            message,
            Some(ModuleCallFindingIdentity::BindingUse {
                call: call.clone(),
                argument: argument.to_string(),
                error: finding.error.clone(),
            }),
        )
    }

    fn has_same_identity(&self, other: &Self) -> bool {
        match (&self.identity, &other.identity) {
            (
                Some(ModuleCallFindingIdentity::ReferenceType(left)),
                Some(ModuleCallFindingIdentity::ReferenceType(right)),
            ) => left.represents_same_finding(right),
            (Some(left), Some(right)) => left == right,
            (None, None) => {
                self.anchor == other.anchor
                    && self.kind == other.kind
                    && self.message == other.message
            }
            (Some(_left), None) => false,
            (None, Some(_right)) => false,
        }
    }

    pub fn kind(&self) -> ModuleCallFindingKind {
        self.kind
    }

    pub fn owners(&self) -> &[ModuleCallFindingOwner] {
        &self.owners
    }

    pub fn diagnostics(&self) -> Vec<crate::parser::Located<&Self>> {
        let mut locations = vec![self.anchor.location().clone()];
        for owner in &self.owners {
            let location = owner.root.location();
            // The primary anchor represents this file. Additional root-call
            // owners retain diagnostics for callers in other files.
            if location.file() != self.anchor.location().file() && !locations.contains(location) {
                locations.push(location.clone());
            }
        }
        locations
            .into_iter()
            .map(|location| crate::parser::Located::new(location, self))
            .collect()
    }

    pub fn anchor(&self) -> &ModuleCallFindingAnchor {
        &self.anchor
    }

    pub fn message(&self) -> &ModuleCallFindingMessage {
        &self.message
    }
}

impl std::fmt::Display for ModuleCallFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source_file = crate::utils::normalize_source_path(self.anchor.source_file.path());
        write!(
            formatter,
            "{} at line {}",
            source_file.display(),
            self.anchor.span.start_line(),
        )
    }
}

impl std::error::Error for ModuleCallFinding {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.message {
            ModuleCallFindingMessage::TypeResolution(error) => Some(error.detail()),
            ModuleCallFindingMessage::AttributeReference(error) => Some(error.as_ref()),
            ModuleCallFindingMessage::UnknownArgument {
                module: _module,
                argument: _argument,
                suggestion: _suggestion,
            } => Some(&self.message),
            ModuleCallFindingMessage::MissingRequiredArgument {
                module: _module,
                argument: _argument,
            } => Some(&self.message),
            ModuleCallFindingMessage::ValueType {
                call_label: _call_label,
                argument: _argument,
                error: _error,
            } => Some(&self.message),
            ModuleCallFindingMessage::HandleSink {
                call_label: _call_label,
                argument: _argument,
                error: _error,
            } => Some(&self.message),
            ModuleCallFindingMessage::BindingUse {
                argument: _argument,
                error: _error,
            } => Some(&self.message),
            ModuleCallFindingMessage::NestedResourceBindingUse(_finding) => Some(&self.message),
            ModuleCallFindingMessage::ReferenceType(_error) => Some(&self.message),
            ModuleCallFindingMessage::Constraint(_diagnostic) => Some(&self.message),
        }
    }
}

/// Whether the authored pass includes resource-reference checks and recursive
/// module loading. Recovery commands skip this phase while retaining direct
/// argument, missing-argument, and unknown-argument validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleCallReferenceChecks {
    Enabled,
    Skipped,
}

/// Input to the authored module-call diagnostic phase.
pub struct ModuleCallCheckInput<
    'a,
    'bindings,
    E,
    C: ?Sized = dyn deferred_types::ProviderTypeCheckContext + 'a,
> {
    pub authored: &'a crate::parser::File<E>,
    pub semantic: Option<&'a crate::parser::File<E>>,
    pub base_dir: Option<&'a crate::paths::ResolvedPath>,
    pub imported_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    pub provider_context: &'a C,
    pub schemas: &'a SchemaRegistry,
    pub binding_use_report: &'a BindingUseReport,
    pub bindings: &'a BindingIndex<'bindings>,
    pub reference_checks: ModuleCallReferenceChecks,
}

/// Typed inputs for checks that become authoritative only after expansion.
pub enum ExpandedModuleCallCheckInput<'a, 'bindings, P: PlaceholderKind = ParserPlaceholder> {
    BindingUses {
        binding_use_report: &'a BindingUseReport,
        compositions: &'a [crate::resource::LiveComposition<P>],
    },
    References {
        compositions: &'a [crate::resource::LiveComposition<P>],
        bindings: &'a BindingIndex<'bindings, AbsoluteBindingAddress>,
    },
    Constraints {
        compositions: &'a [crate::resource::LiveComposition<P>],
        constraint_reports: &'a crate::module_resolver::ModuleCallConstraintReports,
    },
}

struct ModuleCallChecker<'a> {
    source_overrides: crate::config_loader::SourceOverrides,
    provider_context: typecheck::TypecheckProviderContext<'a>,
    schemas: &'a SchemaRegistry,
    findings: Vec<ModuleCallFinding>,
    active_modules: HashSet<crate::paths::CanonicalPath>,
    module_cache: std::collections::HashMap<crate::paths::CanonicalPath, crate::parser::ParsedFile>,
}

struct ModuleFileCheckContext<'a, 'bindings, E> {
    semantic: Option<&'a crate::parser::File<E>>,
    base_dir: Option<&'a crate::paths::ResolvedPath>,
    signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    binding_use_report: &'a BindingUseReport,
    bindings: &'a BindingIndex<'bindings>,
    root_owner: Option<&'a ModuleCallFindingOwnerSource>,
    reference_checks: ModuleCallReferenceChecks,
}

struct ModuleBoundaryCheckContext<'a, 'bindings, E> {
    semantic_call: Option<&'a ModuleCall>,
    authority_file: &'a crate::parser::File<E>,
    signature: &'a crate::module_resolver::ResolvedModuleSignature,
    binding_use_report: &'a BindingUseReport,
    bindings: &'a BindingIndex<'bindings>,
    owner: ModuleCallFindingOwner,
}

struct ModuleReferenceCheckContext<'a, 'bindings, E> {
    semantic_call: Option<&'a ModuleCall>,
    authority_file: &'a crate::parser::File<E>,
    signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    binding_use_report: &'a BindingUseReport,
    bindings: &'a BindingIndex<'bindings>,
    owner: ModuleCallFindingOwner,
}

impl ModuleCallChecker<'_> {
    fn push_finding(&mut self, finding: ModuleCallFinding) {
        if let Some(existing) = self
            .findings
            .iter_mut()
            .find(|existing| existing.has_same_identity(&finding))
        {
            for owner in finding.owners {
                if !existing.owners.contains(&owner) {
                    existing.owners.push(owner);
                }
            }
            return;
        }
        self.findings.push(finding);
    }

    fn push(
        &mut self,
        kind: ModuleCallFindingKind,
        owner: ModuleCallFindingOwner,
        anchor: ModuleCallFindingAnchor,
        message: ModuleCallFindingMessage,
    ) {
        self.push_finding(ModuleCallFinding {
            kind,
            owners: vec![owner],
            anchor,
            message,
            identity: None,
        });
    }

    fn check_file<E>(
        &mut self,
        authored: &crate::parser::File<E>,
        context: ModuleFileCheckContext<'_, '_, E>,
    ) -> Result<(), crate::parser::AnalysisTreeError> {
        let ModuleFileCheckContext {
            semantic,
            base_dir,
            signatures,
            binding_use_report,
            bindings,
            root_owner,
            reference_checks,
        } = context;

        for authored_call in &authored.module_calls {
            let semantic_call = semantic.and_then(|semantic| {
                semantic
                    .module_calls
                    .iter()
                    .find(|candidate| candidate.id() == authored_call.id())
            });
            let authority_file = match (semantic, semantic_call) {
                (Some(semantic), Some(_semantic_call)) => semantic,
                (Some(_semantic), None) => authored,
                (None, None) => authored,
                (None, Some(_semantic_call)) => {
                    unreachable!("a semantic call requires a semantic file")
                }
            };
            let checked_call = semantic_call.unwrap_or(authored_call);
            let root = root_owner.cloned().unwrap_or_else(|| {
                ModuleCallFindingOwnerSource::ModuleCall(authored_call.id().clone())
            });
            let owner = ModuleCallFindingOwner {
                root,
                immediate: ModuleCallFindingOwnerSource::ModuleCall(authored_call.id().clone()),
            };

            self.check_binding_use_findings(authored_call, binding_use_report, &owner);

            if reference_checks == ModuleCallReferenceChecks::Enabled {
                self.check_reference_types(
                    authored_call,
                    ModuleReferenceCheckContext {
                        semantic_call,
                        authority_file,
                        signatures,
                        binding_use_report,
                        bindings,
                        owner: owner.clone(),
                    },
                );
            }
            if let Some(signature) = signatures.get(&authored_call.module_name) {
                self.check_call(
                    authored_call,
                    ModuleBoundaryCheckContext {
                        semantic_call,
                        authority_file,
                        signature,
                        binding_use_report,
                        bindings,
                        owner: owner.clone(),
                    },
                );
            }

            if reference_checks == ModuleCallReferenceChecks::Enabled {
                self.check_nested_module(authority_file, checked_call, base_dir, &owner.root)?;
            }
        }

        if matches!(reference_checks, ModuleCallReferenceChecks::Enabled) {
            for import in &authored.uses {
                let called = authored
                    .module_calls
                    .iter()
                    .any(|call| call.module_name == import.alias);
                if !called {
                    let root = root_owner.cloned().unwrap_or_else(|| {
                        ModuleCallFindingOwnerSource::Authored(module_use_anchor(import))
                    });
                    self.check_nested_import(import, base_dir, &root)?;
                }
            }
        }
        Ok(())
    }

    fn check_binding_use_findings(
        &mut self,
        call: &ModuleCall,
        report: &BindingUseReport,
        owner: &ModuleCallFindingOwner,
    ) {
        for finding in report.findings() {
            let crate::parser::BindingUseOwner::ModuleArgument {
                call: finding_call,
                argument,
                value_span: _,
            } = &finding.use_site.owner
            else {
                continue;
            };
            if finding_call != call.id() {
                continue;
            }

            self.push_binding_use_finding(
                &module_call_label(call),
                finding_call,
                argument,
                finding,
                owner,
            );
        }
    }

    fn push_binding_use_finding(
        &mut self,
        call_label: &str,
        call: &ModuleCallId,
        argument: &str,
        finding: &BindingUseFinding,
        owner: &ModuleCallFindingOwner,
    ) {
        self.push_finding(ModuleCallFinding::binding_use(
            call_label,
            call,
            argument,
            finding,
            owner.clone(),
        ));
    }

    fn check_call<E>(
        &mut self,
        authored_call: &ModuleCall,
        context: ModuleBoundaryCheckContext<'_, '_, E>,
    ) {
        let ModuleBoundaryCheckContext {
            semantic_call,
            authority_file,
            signature,
            binding_use_report,
            bindings,
            owner,
        } = context;

        let call_label = module_call_label(authored_call);
        for authored in authored_call.arguments_with_source_spans() {
            let argument = authored.name();
            let Some(parameter) = signature
                .arguments
                .iter()
                .find(|parameter| parameter.name == argument)
            else {
                let suggestion = signature
                    .arguments
                    .iter()
                    .find(|parameter| {
                        parameter.name.contains(argument) || argument.contains(&parameter.name)
                    })
                    .map(|parameter| parameter.name.clone());
                self.push(
                    ModuleCallFindingKind::UnknownArgument,
                    owner.clone(),
                    ModuleCallFindingAnchor::new(
                        authored_call
                            .id()
                            .argument_key_location(argument)
                            .expect("authored argument key provenance")
                            .clone(),
                    ),
                    ModuleCallFindingMessage::UnknownArgument {
                        module: authored_call.module_name.clone(),
                        argument: argument.to_string(),
                        suggestion,
                    },
                );
                continue;
            };

            if binding_use_authority_owns_module_argument(
                authority_file,
                binding_use_report,
                authored_call.id(),
                argument,
            ) {
                continue;
            }

            let parameter_resolution = match &parameter.static_type {
                StaticType::Known(parameter_type) => resolve_parameter_type_for_check(
                    parameter_type.type_expr(),
                    self.provider_context.as_provider_context(),
                ),
                StaticType::Unchecked(reason) => {
                    if self.provider_context.loaded_context().is_some() {
                        Err(reason.resolution_error().clone())
                    } else {
                        Ok(None)
                    }
                }
            };
            let resolved_parameter = match parameter_resolution {
                Ok(Some(resolved_parameter)) => resolved_parameter,
                Ok(None) => continue,
                Err(error) => {
                    let source_file = authored_call.id().source_file();
                    let line = authored.value_span().start_line();
                    let anchor = ModuleCallFindingAnchor::new(
                        authored_call
                            .id()
                            .argument_key_location(argument)
                            .expect("authored argument key provenance")
                            .clone(),
                    );
                    let diagnostic = TypeResolutionDiagnostic::description(
                        format!(
                            "module {} argument '{}' in {} at line {line}",
                            authored_call.module_name,
                            argument,
                            source_file.display(),
                        ),
                        error.into(),
                        authored_call
                            .id()
                            .argument_key_location(argument)
                            .expect("authored argument key provenance")
                            .clone(),
                    );
                    self.push(
                        ModuleCallFindingKind::TypeResolution,
                        owner.clone(),
                        anchor,
                        ModuleCallFindingMessage::TypeResolution(Box::new(diagnostic)),
                    );
                    continue;
                }
            };
            let semantic_argument = semantic_call.and_then(|call| call.argument(argument));
            let argument_value = semantic_argument.unwrap_or_else(|| authored.argument());
            if let Some(handle) = argument_value.handle() {
                if let Err(error) = check_handle_sink(handle, &resolved_parameter) {
                    self.push_finding(ModuleCallFinding::argument_value(
                        ModuleCallFindingKind::HandleSink,
                        owner.clone(),
                        authored_call.id(),
                        ModuleCallFindingMessage::HandleSink {
                            call_label: call_label.clone(),
                            argument: argument.to_string(),
                            error,
                        },
                        None,
                    ));
                }
                continue;
            }
            if argument_value.pending_parameter().is_some() {
                continue;
            }
            let Some(value) = argument_value.value() else {
                continue;
            };
            if semantic_argument.is_none() && !module_call_value_is_context_free(value) {
                continue;
            }
            if let ResolvedParameterType::Handle(expected) = &resolved_parameter {
                let infer_actual = || {
                    authority_file
                        .expression_report()
                        .expression(
                            authored_call
                                .id()
                                .argument_value_location(argument)
                                .expect("argument provenance"),
                        )
                        .map(|expression| {
                            expression
                                .static_type()
                                .map_or_else(|error| error.to_string(), ToString::to_string)
                        })
                        .unwrap_or_else(|error| error.to_string())
                };
                let reference_check = match value {
                    crate::resource::Value::Deferred(
                        crate::resource::DeferredValue::ResourceRef { path },
                    ) => Some(check_reference_for_handle_sink(
                        bindings,
                        path.into(),
                        expected,
                    )),
                    crate::resource::Value::Deferred(
                        crate::resource::DeferredValue::BindingRef { binding },
                    ) => Some(check_reference_for_handle_sink(
                        bindings,
                        binding.into(),
                        expected,
                    )),
                    _ => None,
                };
                let error = match check_value_for_handle_sink(
                    value,
                    expected,
                    reference_check,
                    infer_actual,
                ) {
                    Ok(_) => continue,
                    Err(error) => error,
                };
                self.push_finding(ModuleCallFinding::argument_value(
                    ModuleCallFindingKind::HandleSink,
                    owner.clone(),
                    authored_call.id(),
                    ModuleCallFindingMessage::HandleSink {
                        call_label: call_label.clone(),
                        argument: argument.to_string(),
                        error,
                    },
                    None,
                ));
                continue;
            }
            let inference = authority_file
                .expression_report()
                .expression(
                    authored_call
                        .id()
                        .argument_value_location(argument)
                        .expect("authored argument expression"),
                )
                .map_err(|error| typecheck::TypeExprValueError::InvalidAccess {
                    error: error.into(),
                });
            let location = authored_call
                .id()
                .argument_value_location(argument)
                .expect("authored argument expression");
            let reports_here = !self.findings.iter().any(|finding| finding.anchor.location() == location && finding.owners.contains(&owner))
                && !inference.as_ref().is_ok_and(|expression| {
                    matches!(expression.static_type(), Err(crate::static_type::InferenceIssue::Failed(error))
                        if authority_file.expression_report().findings().iter().any(|finding| finding.detail() == &error))
                });
            if let Err(error) = inference.and_then(|expression| {
                self.provider_context.check_at(
                    &resolved_parameter.as_type_expr(),
                    value,
                    expression,
                    deferred_types::ProviderTypeCheckSite::new(
                        authored_call
                            .id()
                            .argument_value_location(argument)
                            .expect("parser-owned argument value")
                            .clone(),
                        format!("{call_label} argument '{argument}'"),
                    ),
                )
            }) && reports_here
            {
                self.push_finding(ModuleCallFinding::argument_value(
                    ModuleCallFindingKind::ValueType,
                    owner.clone(),
                    authored_call.id(),
                    ModuleCallFindingMessage::ValueType {
                        call_label: call_label.clone(),
                        argument: argument.to_string(),
                        error: Box::new(error),
                    },
                    None,
                ));
            }
        }

        for parameter in &signature.arguments {
            if parameter.default.is_none()
                && !authored_call.contains_argument(&parameter.name)
                && !module_argument_has_pending_binding_use(
                    authority_file,
                    authored_call.id(),
                    &parameter.name,
                )
            {
                self.push(
                    ModuleCallFindingKind::MissingRequiredArgument,
                    owner.clone(),
                    ModuleCallFindingAnchor::new(authored_call.id().declaration_location().clone()),
                    ModuleCallFindingMessage::MissingRequiredArgument {
                        module: authored_call.module_name.clone(),
                        argument: parameter.name.clone(),
                    },
                );
            }
        }
    }

    fn check_reference_types<E>(
        &mut self,
        authored_call: &ModuleCall,
        context: ModuleReferenceCheckContext<'_, '_, E>,
    ) {
        let ModuleReferenceCheckContext {
            semantic_call,
            authority_file,
            signatures,
            binding_use_report,
            bindings,
            owner,
        } = context;
        let reference_call = semantic_call.unwrap_or(authored_call);
        for error in validate_module_call_argument_ref_types_inner(
            std::slice::from_ref(reference_call),
            signatures,
            bindings,
            UnknownReferencePolicy::Report,
            Some(authority_file.expression_report()),
        ) {
            if signatures
                .get(&authored_call.module_name)
                .is_some_and(|signature| {
                    !signature
                        .arguments
                        .iter()
                        .any(|parameter| parameter.name == error.identity.argument)
                })
                || binding_use_authority_owns_module_argument(
                    authority_file,
                    binding_use_report,
                    authored_call.id(),
                    &error.identity.argument,
                )
                || signatures
                    .get(&authored_call.module_name)
                    .and_then(|signature| {
                        signature
                            .arguments
                            .iter()
                            .find(|parameter| parameter.name == error.identity.argument)
                    })
                    .and_then(|parameter| parameter.static_type.known())
                    .and_then(|type_expr| {
                        resolve_parameter_type_for_check(
                            type_expr,
                            self.provider_context.as_provider_context(),
                        )
                        .ok()
                        .flatten()
                    })
                    .is_some_and(|parameter| parameter.is_handle())
                || reference_call
                    .argument(&error.identity.argument)
                    .is_some_and(|argument| argument.handle().is_some())
            {
                continue;
            }
            self.push_finding(ModuleCallFinding::reference_type(
                &module_call_label(authored_call),
                error.identity,
                owner.clone(),
            ));
        }
    }

    fn check_nested_module<E>(
        &mut self,
        file: &crate::parser::File<E>,
        call: &ModuleCall,
        base_dir: Option<&crate::paths::ResolvedPath>,
        root_owner: &ModuleCallFindingOwnerSource,
    ) -> Result<(), crate::parser::AnalysisTreeError> {
        let Some(import) = file
            .uses
            .iter()
            .find(|import| import.alias == call.module_name)
        else {
            return Ok(());
        };
        self.check_nested_import(import, base_dir, root_owner)?;
        Ok(())
    }

    fn check_nested_import(
        &mut self,
        import: &crate::parser::UseStatement,
        base_dir: Option<&crate::paths::ResolvedPath>,
        root_owner: &ModuleCallFindingOwnerSource,
    ) -> Result<(), crate::parser::AnalysisTreeError> {
        let Some(base_dir) = base_dir else {
            return Ok(());
        };
        let Ok(module_dir) = base_dir.join(&import.path) else {
            return Ok(());
        };
        let guard = module_dir.identity();
        if !self.active_modules.insert(guard.clone()) {
            return Ok(());
        }

        // An enriched parse intentionally rejects an unknown bare type before
        // this checker can turn it into a contextual declaration diagnostic.
        // Retain the providerless syntax snapshot in that case, then let the
        // typed resolver below own the error. The strict expansion pass still
        // rejects the same declaration; its parser backstop is deduplicated
        // against this source-anchored finding.
        if let Some(mut module) = self.module_cache.get(&guard).cloned().or_else(|| {
            crate::module_resolver::loader::load_module_with_overrides(
                &module_dir,
                self.provider_context.as_provider_context(),
                &self.source_overrides,
            )
            .or_else(|| {
                crate::module_resolver::loader::load_module_with_overrides(
                    &module_dir,
                    &ProviderContext::new(
                        self.provider_context
                            .as_provider_context()
                            .environment
                            .clone(),
                    ),
                    &self.source_overrides,
                )
            })
        }) {
            let resolution_errors = resolve_file_type_exprs(
                &mut module,
                self.provider_context
                    .session()
                    .expect("module checks borrow their run session"),
            )?;
            let signatures = crate::module_resolver::load_resolved_module_signatures_with_overrides(
                &module,
                &module_dir,
                self.provider_context.as_provider_context(),
                &self.source_overrides,
            );
            let bindings = BindingIndex::from_parsed_with_module_signatures(
                &module,
                self.schemas,
                &signatures,
                self.provider_context.as_provider_context(),
            );
            let judged = crate::config_loader::load_buffer_configuration(
                &module,
                self.schemas,
                self.provider_context.as_provider_context(),
            );
            self.module_cache
                .insert(guard.clone(), judged.parsed().clone());
            let binding_use_report =
                check_bare_binding_uses(&ValidationTypeContext::with_provider_context(
                    judged.judged(),
                    self.provider_context,
                    &signatures,
                ));
            for finding in binding_use_report.resource_findings() {
                let anchor = ModuleCallFindingAnchor::new(finding.use_site.location().clone());
                self.push(
                    ModuleCallFindingKind::BindingUse,
                    ModuleCallFindingOwner {
                        root: root_owner.clone(),
                        immediate: ModuleCallFindingOwnerSource::Authored(anchor.clone()),
                    },
                    anchor,
                    ModuleCallFindingMessage::NestedResourceBindingUse(Box::new(finding.clone())),
                );
            }
            for error in resolution_errors {
                let anchor = ModuleCallFindingAnchor::new(error.location().clone());
                self.push(
                    ModuleCallFindingKind::TypeResolution,
                    ModuleCallFindingOwner {
                        root: root_owner.clone(),
                        immediate: ModuleCallFindingOwnerSource::Authored(anchor.clone()),
                    },
                    anchor,
                    ModuleCallFindingMessage::TypeResolution(Box::new(error)),
                );
            }
            self.check_file(
                &module,
                ModuleFileCheckContext {
                    semantic: Some(&module),
                    base_dir: Some(&module_dir),
                    signatures: &signatures,
                    binding_use_report: &binding_use_report,
                    bindings: &bindings,
                    root_owner: Some(root_owner),
                    reference_checks: ModuleCallReferenceChecks::Enabled,
                },
            )?;
        }

        self.active_modules.remove(&guard);
        Ok(())
    }

    fn check_expanded<P: PlaceholderKind>(
        &mut self,
        compositions: &[crate::resource::LiveComposition<P>],
        bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
    ) {
        for composition in compositions {
            let call_path = composition.call_path();
            let root = call_path.root();
            let immediate = call_path.immediate();
            for (argument_name, argument) in &composition.signature.arguments {
                let (declared_type, _) = argument.validation_metadata();
                let Some(TypeExpr::Ref(expected)) = declared_type.known() else {
                    continue;
                };
                if argument.forwarded_parameter().is_some() {
                    // A nested module may retain an outer parameter until a
                    // concrete caller supplies it. The structural Parameter
                    // arm records that phase explicitly; its traversal value
                    // is only parser metadata and must not be diagnosed as an
                    // ordinary unknown value. The prefixed composition and
                    // eventual call boundary check the supplied operand.
                    continue;
                }
                let error = if let Some(handle) = argument.handle() {
                    check_handle_sink(handle, &ResolvedParameterType::Handle(expected.clone()))
                        .err()
                } else {
                    Some(HandleSinkError::expanded_value_for_handle(
                        expected,
                        argument.value(),
                        bindings,
                    ))
                };
                let Some(error) = error else {
                    continue;
                };
                self.push_finding(ModuleCallFinding::argument_value(
                    ModuleCallFindingKind::HandleSink,
                    ModuleCallFindingOwner {
                        root: ModuleCallFindingOwnerSource::ModuleCall(root.id.clone()),
                        immediate: ModuleCallFindingOwnerSource::ModuleCall(immediate.id.clone()),
                    },
                    &immediate.id,
                    ModuleCallFindingMessage::HandleSink {
                        call_label: immediate.display_label(),
                        argument: argument_name.clone(),
                        error,
                    },
                    None,
                ));
            }
        }

        for error in validate_composition_ref_types_with_bindings(compositions, bindings) {
            match error {
                CompositionRefError::ModuleCall(error) => {
                    let root = error.call_path.root();
                    let immediate = error.call_path.immediate();
                    let display_path = error.path.render(
                        |path| ModuleCallDiagnosticPath::Expanded(path.clone()),
                        || ModuleCallDiagnosticPath::Protected,
                    );
                    self.push_finding(ModuleCallFinding::reference_type_with_display_path(
                        &immediate.display_label(),
                        ModuleCallReferenceIdentity {
                            call: immediate.id.clone(),
                            argument: error.argument,
                            path: display_path.clone(),
                            reference_position: Some(error.reference_position),
                            binding_use: None,
                            kind: error.kind,
                        },
                        display_path,
                        ModuleCallFindingOwner {
                            root: ModuleCallFindingOwnerSource::ModuleCall(root.id.clone()),
                            immediate: ModuleCallFindingOwnerSource::ModuleCall(
                                immediate.id.clone(),
                            ),
                        },
                        false,
                    ));
                }
                CompositionRefError::Attribute(error) => {
                    let Some(first_path) = error.call_paths.first() else {
                        unreachable!("expanded attribute errors retain their call path")
                    };
                    let first_immediate = first_path.immediate();
                    let anchor = ModuleCallFindingAnchor::new(
                        first_immediate.id.declaration_location().clone(),
                    );
                    for call_path in &error.call_paths {
                        self.push(
                            ModuleCallFindingKind::ReferenceType,
                            ModuleCallFindingOwner {
                                root: ModuleCallFindingOwnerSource::ModuleCall(
                                    call_path.root().id.clone(),
                                ),
                                immediate: ModuleCallFindingOwnerSource::ModuleCall(
                                    call_path.immediate().id.clone(),
                                ),
                            },
                            anchor.clone(),
                            ModuleCallFindingMessage::AttributeReference(Box::new(error.clone())),
                        );
                    }
                }
            }
        }
    }

    fn check_expanded_binding_uses<P: PlaceholderKind>(
        &mut self,
        report: &BindingUseReport,
        compositions: &[crate::resource::LiveComposition<P>],
    ) {
        for finding in report.findings() {
            let crate::parser::BindingUseOwner::ModuleArgument {
                call,
                argument,
                value_span: _,
            } = &finding.use_site.owner
            else {
                continue;
            };
            let mut owners = Vec::new();
            for composition in compositions {
                let path = composition.call_path();
                if let Some(immediate) = path.calls().find(|candidate| candidate.id == *call) {
                    let owner = ModuleCallFindingOwner {
                        root: ModuleCallFindingOwnerSource::ModuleCall(path.root().id.clone()),
                        immediate: ModuleCallFindingOwnerSource::ModuleCall(immediate.id.clone()),
                    };
                    if !owners.iter().any(|(existing, _label)| existing == &owner) {
                        owners.push((owner, immediate.display_label()));
                    }
                }
            }
            if owners.is_empty() {
                owners.push((
                    ModuleCallFindingOwner {
                        root: ModuleCallFindingOwnerSource::ModuleCall(call.clone()),
                        immediate: ModuleCallFindingOwnerSource::ModuleCall(call.clone()),
                    },
                    "anonymous module call".to_string(),
                ));
            }
            for (owner, call_label) in owners {
                self.push_binding_use_finding(&call_label, call, argument, finding, &owner);
            }
        }
    }

    fn check_constraints<P: PlaceholderKind>(
        &mut self,
        reports: &crate::module_resolver::ModuleCallConstraintReports,
        compositions: &[crate::resource::LiveComposition<P>],
    ) {
        for report in reports {
            let report = match report {
                crate::module_resolver::ConstraintScopeReport::Directory(failures) => {
                    for failure in failures {
                        let anchor = ModuleCallFindingAnchor::new(failure.location().clone());
                        let owner = ModuleCallFindingOwnerSource::Authored(anchor.clone());
                        self.push_finding(ModuleCallFinding {
                            kind: ModuleCallFindingKind::Constraint,
                            owners: vec![ModuleCallFindingOwner {
                                root: owner.clone(),
                                immediate: owner,
                            }],
                            anchor,
                            message: ModuleCallFindingMessage::Constraint(
                                crate::module_resolver::ConstraintDiagnostic::Directory(
                                    failure.detail().clone(),
                                ),
                            ),
                            identity: None,
                        });
                    }
                    continue;
                }
                crate::module_resolver::ConstraintScopeReport::ModuleCall(report) => report,
            };
            for outcome in &report.outcomes {
                let crate::module_resolver::ResolvedModuleConstraintStatus::Failed {
                    diagnostic,
                    origin,
                } = &outcome.status
                else {
                    continue;
                };
                let authored_argument = matches!(
                    origin,
                    crate::module_resolver::ResolvedModuleConstraintOrigin::Argument(
                        crate::parser::ArgumentOrigin::Authored { .. }
                    )
                );
                let mut owners = compositions
                    .iter()
                    .filter_map(|composition| {
                        let path = composition.call_path();
                        (path.immediate().id == report.source.id
                            && constraint_report_instance_matches(
                                &path.immediate().instance,
                                &report.instance,
                            ))
                        .then(|| ModuleCallFindingOwner {
                            root: ModuleCallFindingOwnerSource::ModuleCall(path.root().id.clone()),
                            immediate: ModuleCallFindingOwnerSource::ModuleCall(
                                path.immediate().id.clone(),
                            ),
                        })
                    })
                    .collect::<Vec<_>>();
                owners.dedup();
                if owners.is_empty() {
                    owners.push(ModuleCallFindingOwner {
                        root: ModuleCallFindingOwnerSource::ModuleCall(report.source.id.clone()),
                        immediate: ModuleCallFindingOwnerSource::ModuleCall(
                            report.source.id.clone(),
                        ),
                    });
                }
                for owner in owners {
                    let message = ModuleCallFindingMessage::Constraint(
                        crate::module_resolver::ConstraintDiagnostic::ModuleCall(
                            diagnostic.clone(),
                        ),
                    );
                    match authored_argument {
                        true => self.push_finding(ModuleCallFinding::argument_value(
                            ModuleCallFindingKind::Constraint,
                            owner,
                            &report.source.id,
                            message,
                            None,
                        )),
                        false => self.push(
                            ModuleCallFindingKind::Constraint,
                            owner,
                            ModuleCallFindingAnchor::new(
                                report.source.id.declaration_location().clone(),
                            ),
                            message,
                        ),
                    }
                }
            }
        }
    }
}

/// A constraint evaluated while loading an imported module carries that
/// module-local instance path. Each outer expansion prefixes the path but does
/// not change the concrete outcome; match the exact instance or a dot-delimited
/// expansion of it. Constraints that depended on outer arguments are evaluated
/// later and therefore take the exact branch with their fully prefixed path.
fn constraint_report_instance_matches(expanded: &str, reported: &str) -> bool {
    expanded == reported
        || expanded
            .strip_suffix(reported)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn module_use_anchor(import: &crate::parser::UseStatement) -> ModuleCallFindingAnchor {
    ModuleCallFindingAnchor::new(import.declaration_location().clone())
}

fn module_call_label(call: &ModuleCall) -> String {
    crate::resource::CompositionCall::<RelativeBindingName>::display_label_for_module_call(call)
}

fn module_call_value_is_context_free(value: &Value) -> bool {
    match value {
        crate::resource::Value::Deferred(crate::resource::DeferredValue::ModuleIf(_)) => false,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(_string)) => true,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(
            _canonical,
        )) => true,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::Int(_integer)) => true,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::Float(_float)) => true,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::Bool(_boolean)) => true,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::Duration(_duration)) => {
            true
        }
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::StringList(_strings)) => {
            true
        }
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::EnumIdentifier(
            _identifier,
        )) => false,
        crate::resource::Value::Deferred(crate::resource::DeferredValue::ResourceRef {
            path: _path,
        }) => false,
        crate::resource::Value::Deferred(crate::resource::DeferredValue::BindingRef {
            binding: _binding,
        }) => false,
        crate::resource::Value::Deferred(crate::resource::DeferredValue::FunctionCall {
            name: _name,
            args: _args,
            source_line: _source_line,
        }) => false,
        crate::resource::Value::Deferred(crate::resource::DeferredValue::Unavailable {
            name: _name,
            args: _args,
            reason: _reason,
            source_line: _source_line,
        }) => false,
        crate::resource::Value::Deferred(
            crate::resource::DeferredValue::ResolvedFunctionCall {
                function: _function,
                functions: _functions,
                args: _args,
                source_line: _source_line,
            },
        ) => false,
        crate::resource::Value::Deferred(crate::resource::DeferredValue::Access(_access)) => false,
        crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(_reason)) => false,

        crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
            _placeholder,
        )) => false,
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(items)) => {
            items.iter().all(module_call_value_is_context_free)
        }
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(entries)) => {
            entries.values().all(module_call_value_is_context_free)
        }
        crate::resource::Value::Deferred(crate::resource::DeferredValue::Interpolation(parts)) => {
            parts.iter().all(|part| {
                matches!(part, crate::resource::InterpolationPart::Literal(_literal))
                    || matches!(part, crate::resource::InterpolationPart::Expr(value)
                    if module_call_value_is_context_free(value))
            })
        }
        crate::resource::Value::Deferred(crate::resource::DeferredValue::Secret(inner)) => {
            inner.visit(module_call_value_is_context_free)
        }
        crate::resource::Value::Deferred(crate::resource::DeferredValue::Coalesce {
            value,
            default,
        }) => {
            module_call_value_is_context_free(value) && module_call_value_is_context_free(default)
        }
        crate::resource::Value::Deferred(crate::resource::DeferredValue::IntegerArithmetic(
            expression,
        )) => {
            module_call_value_is_context_free(expression.lhs())
                && module_call_value_is_context_free(expression.rhs())
        }
    }
}

/// Collect module-call findings from authored source and its imported graph.
pub fn check_module_calls<E>(
    input: ModuleCallCheckInput<'_, '_, E, dyn deferred_types::ProviderTypeCheckContext + '_>,
) -> Result<Vec<ModuleCallFinding>, crate::parser::AnalysisTreeError> {
    check_module_calls_with_overrides(input, &Default::default())
}

/// Check a sealed directory with its authoritative upstream facts. The same
/// findings can then be reused for module-resolution ownership and publication.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, validation::{check_module_calls_from_analysis, deferred_types::ProviderTypeCheckContext}, paths::ResolvedPath, schema::SchemaRegistry, upstream_exports::UpstreamExports, config_loader::SourceOverrides};
/// fn reject_raw(file: &ParsedFile, base: &ResolvedPath, context: &dyn ProviderTypeCheckContext, schemas: &SchemaRegistry, exports: &UpstreamExports, overrides: &SourceOverrides) {
///     check_module_calls_from_analysis(file, base, context, schemas, exports, overrides);
/// }
/// ```
pub fn check_module_calls_from_analysis(
    tree: &crate::parser::AnalysisTree<'_>,
    base: &crate::paths::ResolvedPath,
    provider_context: &dyn deferred_types::ProviderTypeCheckContext,
    schemas: &SchemaRegistry,
    upstream_exports: &crate::upstream_exports::UpstreamExports,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<Vec<ModuleCallFinding>, crate::parser::AnalysisTreeError> {
    let parsed = tree.file();
    let signatures = crate::module_resolver::load_resolved_module_signatures_with_overrides(
        parsed,
        base,
        provider_context.catalog(),
        overrides,
    );
    let judged = crate::config_loader::load_buffer_configuration(
        parsed,
        schemas,
        provider_context.catalog(),
    );
    let context = ValidationTypeContext::in_session(judged.judged(), provider_context, &signatures);
    let binding_use_report = check_bare_binding_uses(&context);
    let bindings = BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(
        parsed,
        schemas,
        Some(upstream_exports),
        &signatures,
        provider_context,
    );
    check_module_calls_with_overrides(
        ModuleCallCheckInput {
            authored: parsed,
            semantic: Some(parsed),
            base_dir: Some(base),
            imported_signatures: &signatures,
            provider_context,
            schemas,
            binding_use_report: &binding_use_report,
            bindings: &bindings,
            reference_checks: ModuleCallReferenceChecks::Enabled,
        },
        overrides,
    )
}

pub fn check_module_calls_with_overrides<E>(
    input: ModuleCallCheckInput<'_, '_, E, dyn deferred_types::ProviderTypeCheckContext + '_>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<Vec<ModuleCallFinding>, crate::parser::AnalysisTreeError> {
    let provider_context = typecheck::TypecheckProviderContext::for_session(input.provider_context);
    check_module_calls_with_provider_context(input, provider_context, overrides)
}

/// Providerless module-call diagnostics for bootstrap and orphan-editor
/// contexts. Structural checks still run, while provider-owned leaves are not
/// sent to an unloaded resolver.
pub fn check_module_calls_providerless<E>(
    input: ModuleCallCheckInput<'_, '_, E, dyn deferred_types::ProviderTypeCheckContext + '_>,
) -> Result<Vec<ModuleCallFinding>, crate::parser::AnalysisTreeError> {
    let provider_context = typecheck::TypecheckProviderContext::for_session(input.provider_context);
    check_module_calls_with_provider_context(input, provider_context, &Default::default())
}

fn check_module_calls_with_provider_context<'a, 'bindings, E, C: ?Sized>(
    input: ModuleCallCheckInput<'a, 'bindings, E, C>,
    provider_context: typecheck::TypecheckProviderContext<'a>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<Vec<ModuleCallFinding>, crate::parser::AnalysisTreeError> {
    let ModuleCallCheckInput {
        authored,
        semantic,
        base_dir,
        imported_signatures,
        provider_context: _,
        schemas,
        binding_use_report,
        bindings,
        reference_checks,
    } = input;
    let mut checker = ModuleCallChecker {
        source_overrides: overrides.clone(),
        provider_context,
        schemas,
        findings: Vec::new(),
        active_modules: HashSet::new(),
        module_cache: std::collections::HashMap::new(),
    };
    checker.check_file(
        authored,
        ModuleFileCheckContext {
            semantic,
            base_dir,
            signatures: imported_signatures,
            binding_use_report,
            bindings,
            root_owner: None,
            reference_checks,
        },
    )?;
    Ok(checker.findings)
}

/// Collect only findings whose authoritative inputs exist after expansion.
pub fn check_expanded_module_calls<P: PlaceholderKind>(
    input: ExpandedModuleCallCheckInput<'_, '_, P>,
    provider_context: &ProviderContext,
) -> Vec<ModuleCallFinding> {
    let schemas = SchemaRegistry::new();
    let overrides = Default::default();
    let mut checker = ModuleCallChecker {
        source_overrides: overrides,
        provider_context: typecheck::TypecheckProviderContext::providerless(provider_context),
        schemas: &schemas,
        findings: Vec::new(),
        active_modules: HashSet::new(),
        module_cache: std::collections::HashMap::new(),
    };
    match input {
        ExpandedModuleCallCheckInput::BindingUses {
            binding_use_report,
            compositions,
        } => checker.check_expanded_binding_uses(binding_use_report, compositions),
        ExpandedModuleCallCheckInput::References {
            compositions,
            bindings,
        } => checker.check_expanded(compositions, bindings),
        ExpandedModuleCallCheckInput::Constraints {
            compositions,
            constraint_reports,
        } => checker.check_constraints(constraint_reports, compositions),
    }
    checker.findings
}

/// Merge authored and expanded results using the same source-aware identity
/// as the individual phases while retaining every distinct root owner.
pub fn merge_module_call_findings(
    groups: impl IntoIterator<Item = Vec<ModuleCallFinding>>,
) -> Vec<ModuleCallFinding> {
    let mut merged: Vec<ModuleCallFinding> = Vec::new();
    for finding in groups.into_iter().flatten() {
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| existing.has_same_identity(&finding))
        {
            if let (
                ModuleCallFindingMessage::ReferenceType(existing_diagnostic),
                ModuleCallFindingMessage::ReferenceType(incoming_diagnostic),
            ) = (&mut existing.message, &finding.message)
                && matches!(
                    incoming_diagnostic.display_path,
                    ModuleCallDiagnosticPath::Expanded(_)
                )
            {
                **existing_diagnostic = (**incoming_diagnostic).clone();
                existing.identity = finding.identity.clone();
            }
            for owner in finding.owners {
                if !existing.owners.contains(&owner) {
                    existing.owners.push(owner);
                }
            }
        } else {
            merged.push(finding);
        }
    }
    merged
}

/// Export parameters checked against the pre-apply binding view.
///
/// This deliberately retains both the original expressions and a resolved
/// copy used only for plan display. A pre-apply resource binding can contain
/// attributes copied from current state; replacing the original expression
/// with that value would make post-apply export writeback persist stale data.
/// Keeping the two representations behind a distinct type prevents the
/// display snapshot from being passed to the post-apply writeback boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportsCheckedPreApply<P> {
    original_params: Vec<P>,
    display_params: Vec<P>,
}

impl<P> ExportsCheckedPreApply<P> {
    /// Values resolved against the pre-apply binding view for plan display.
    pub fn display_values(&self) -> &[P] {
        &self.display_params
    }

    pub fn is_empty(&self) -> bool {
        self.original_params.is_empty()
    }
}

/// Export parameters resolved against the post-apply binding view and proven
/// not to contain a secret.
///
/// The inner vector is private and the only constructor requires an
/// [`ExportsCheckedPreApply`] proof. State serialization accepts this type,
/// never the pre-apply display snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedExportValues<P> {
    params: Vec<P>,
}

impl<P> CheckedExportValues<P> {
    pub fn as_slice(&self) -> &[P] {
        &self.params
    }

    pub fn iter(&self) -> std::slice::Iter<'_, P> {
        self.params.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }
}

fn resolve_and_validate_export_values<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &[E],
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Vec<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    resolve_and_validate_export_values_report(export_params, bindings, config).map_err(|findings| {
        finding_messages(findings.into_iter().map(|finding| finding.into_parts().1))
    })
}

fn resolve_and_validate_export_values_report<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &[E],
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Vec<E>, Vec<crate::parser::Located<ParameterValueFinding>>>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let mut checked = Vec::with_capacity(export_params.len());
    let mut errors = Vec::new();

    for param in export_params {
        let mut resolved_param = param.clone();
        let located =
            |finding| crate::parser::Located::new(param.type_check_location().clone(), finding);
        if let Some(value) = param.value() {
            let mut references_removed_secret = false;
            value.visit_resource_refs(&mut |path, usage| {
                if usage == crate::resource::ResourceRefUsage::Required
                    && path.removed_attribute_was_sensitive(bindings)
                {
                    references_removed_secret = true;
                }
            });
            if references_removed_secret {
                errors.push(located(ParameterValueFinding::SecretExport {
                    name: param.name().to_string(),
                }));
                checked.push(resolved_param);
                continue;
            }
            match crate::resolver::resolve_ref_value_with_config(value, bindings, config) {
                Ok(resolved) if crate::value::contains_secret(&resolved) => {
                    errors.push(located(ParameterValueFinding::SecretExport {
                        name: param.name().to_string(),
                    }));
                }
                Ok(resolved) => {
                    resolved_param.replace_value(resolved);
                    if let (Some(type_expr), Some(value)) =
                        (resolved_param.type_expr_opt(), resolved_param.value())
                    {
                        let expression = match param.judgement() {
                            Ok(expression) => expression,
                            Err(error) => {
                                errors.push(located(ParameterValueFinding::invalid(
                                    "export",
                                    param.name(),
                                    error.clone(),
                                    None,
                                )));
                                checked.push(resolved_param);
                                continue;
                            }
                        };
                        let provider = typecheck::TypecheckProviderContext::for_session(config);
                        let check = provider.check_at(
                            type_expr,
                            value,
                            expression,
                            deferred_types::ProviderTypeCheckSite::new(
                                param.type_check_location().clone(),
                                format!("export '{}'", param.name()),
                            ),
                        );
                        if let Err(error) = check {
                            errors.push(located(ParameterValueFinding::invalid(
                                "export",
                                param.name(),
                                error,
                                None,
                            )));
                        }
                    }
                }
                Err(error) => errors.push(located(ParameterValueFinding::invalid(
                    "export",
                    param.name(),
                    Box::new(error),
                    None,
                ))),
            }
        }
        checked.push(resolved_param);
    }

    if errors.is_empty() {
        Ok(checked)
    } else {
        Err(errors)
    }
}

/// Resolve and structurally validate exports without loaded providers.
/// Provider-owned custom-type leaves are left unchecked, while container
/// shapes and provider-independent sibling values retain normal diagnostics.
pub fn check_export_values_pre_apply_providerless<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &[E],
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<ExportsCheckedPreApply<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let display_params = resolve_and_validate_export_values(export_params, bindings, config)?;
    Ok(ExportsCheckedPreApply {
        original_params: export_params.to_vec(),
        display_params,
    })
}

/// Resolve every export through the pre-apply value binding graph, evaluate
/// builtins that become concrete, and accumulate both secret-boundary and
/// value-type errors in one pass.
pub fn check_export_values_pre_apply<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &[E],
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<ExportsCheckedPreApply<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let display_params = resolve_and_validate_export_values(export_params, bindings, config)?;
    Ok(ExportsCheckedPreApply {
        original_params: export_params.to_vec(),
        display_params,
    })
}

/// Collect one typed pre-apply export failure per authored boundary.
pub fn check_export_values_pre_apply_report<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &[E],
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<ExportsCheckedPreApply<E>, Vec<crate::parser::Located<ParameterValueFinding>>>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let display_params =
        resolve_and_validate_export_values_report(export_params, bindings, config)?;
    Ok(ExportsCheckedPreApply {
        original_params: export_params.to_vec(),
        display_params,
    })
}

/// Resolve the original export expressions against the post-apply binding
/// view. This is the only constructor for [`CheckedExportValues`], so a
/// pre-apply display snapshot cannot be serialized into state by mistake.
pub fn resolve_export_values_post_apply<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &ExportsCheckedPreApply<E>,
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<CheckedExportValues<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let params =
        resolve_and_validate_export_values(&export_params.original_params, bindings, config)?;
    Ok(CheckedExportValues { params })
}

/// Validate concrete module attribute values against their declared types.
///
/// Reference-valued attributes are deferred here and checked separately by
/// [`validate_attribute_param_ref_types_with_bindings`], whose binding index
/// has the source type authority needed for directional checks.
pub fn validate_attribute_params<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    config: &LoadedProviderContext,
) -> Result<(), String> {
    validate_attribute_params_impl(
        attribute_params,
        typecheck::TypecheckProviderContext::loaded(config),
    )
}

pub fn validate_attribute_params_with_context<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), String> {
    validate_attribute_params_impl(
        attribute_params,
        typecheck::TypecheckProviderContext::for_session(config),
    )
}

fn validate_attribute_params_impl<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    config: typecheck::TypecheckProviderContext<'_>,
) -> Result<(), String> {
    parameter_findings_result(
        attribute_value_findings_impl(attribute_params, config)
            .into_iter()
            .map(|finding| finding.into_parts().1)
            .collect(),
    )
}

pub fn attribute_value_findings<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Vec<crate::parser::Located<ParameterValueFinding>> {
    attribute_value_findings_impl(
        attribute_params,
        typecheck::TypecheckProviderContext::for_session(config),
    )
}

fn attribute_value_findings_impl<P: PlaceholderKind, B: BindingAddress>(
    attribute_params: &[crate::parser::AttributeParameter<TypeExpr, P, B>],
    config: typecheck::TypecheckProviderContext<'_>,
) -> Vec<crate::parser::Located<ParameterValueFinding>> {
    let mut errors = Vec::new();

    for param in attribute_params {
        let invalid = |error, line| {
            crate::parser::Located::new(
                param.type_check_location().clone(),
                ParameterValueFinding::invalid("attribute", &param.name, error, line),
            )
        };
        let (Some(type_expr), Some(expression)) = (&param.static_type, &param.value) else {
            continue;
        };
        match expression {
            crate::parser::BoundaryExpression::Value(value) => {
                if let TypeExpr::Ref(expected) = type_expr {
                    match value {
                        // The BindingIndex reference pass owns the source's
                        // named identity type. Deferring these shapes avoids a
                        // coarse "resource attribute value" duplicate.
                        crate::resource::Value::Deferred(
                            crate::resource::DeferredValue::ResourceRef { .. }
                            | crate::resource::DeferredValue::BindingRef { .. }
                            | crate::resource::DeferredValue::Placeholder(_)
                            | crate::resource::DeferredValue::Access(_),
                        ) => continue,
                        crate::resource::Value::Concrete(_)
                        | crate::resource::Value::Deferred(
                            crate::resource::DeferredValue::FunctionCall { .. }
                            | crate::resource::DeferredValue::Unavailable { .. }
                            | crate::resource::DeferredValue::ResolvedFunctionCall { .. }
                            | crate::resource::DeferredValue::Interpolation(_)
                            | crate::resource::DeferredValue::Secret(_)
                            | crate::resource::DeferredValue::Unknown(_)
                            | crate::resource::DeferredValue::Coalesce { .. }
                            | crate::resource::DeferredValue::ModuleIf(_)
                            | crate::resource::DeferredValue::IntegerArithmetic(_),
                        ) => {
                            errors.push(invalid(
                                ParameterValueError::ExpectedHandle {
                                    expected: expected.clone(),
                                    actual: typecheck::value_kind_name(value),
                                },
                                None,
                            ));
                            continue;
                        }
                    }
                }
                let expression = match param.judgement() {
                    Ok(expression) => expression,
                    Err(error) => {
                        errors.push(invalid(error.clone().into(), None));
                        continue;
                    }
                };
                if let Err(error) = config.check_boundary_values_at(
                    type_expr,
                    value,
                    expression,
                    deferred_types::ProviderTypeCheckSite::new(
                        param.type_check_location().clone(),
                        format!("attribute '{}'", param.name),
                    ),
                ) {
                    errors.push(invalid(error.into(), None));
                }
            }
            crate::parser::BoundaryExpression::Handle(handle) => {
                let resolved =
                    match resolve_parameter_type_for_check(type_expr, config.as_provider_context())
                    {
                        Ok(Some(resolved)) => resolved,
                        Ok(None) => continue,
                        Err(error) => {
                            errors.push(invalid(
                                error.into(),
                                Some(param.source_span().start_line()),
                            ));
                            continue;
                        }
                    };
                match &resolved {
                    ResolvedParameterType::Handle(_) => {
                        if let Err(error) =
                            check_handle_sink(&handle.clone().into_structural(), &resolved)
                        {
                            errors.push(invalid(
                                error.into(),
                                Some(param.source_span().start_line()),
                            ));
                        }
                    }
                    ResolvedParameterType::Value(expected) => {
                        let types = MismatchTypeNames::for_type_exprs(
                            expected,
                            &TypeExpr::Ref(handle.resource_type().clone()),
                        );
                        errors.push(invalid(
                            ParameterValueError::HandleForValue(types),
                            Some(param.source_span().start_line()),
                        ));
                    }
                }
            }
        }
    }

    errors
}

/// Check for unused `let` bindings and return the unused binding names.
///
/// A binding is unused if its name never appears as a value-carried reference
/// (attribute refs, interpolation expressions, function arguments, or secret
/// inner values) or parser-recorded bare use in resource attributes, module
/// call arguments, attribute parameters, export parameters, or an `import.id`;
/// as a `directives.depends_on` or wait target/dependency; or as the configured
/// resource matched by an `import.to` or `moved.to` address.
/// `moved.from` and `removed.from` identify state rows absent from the desired
/// configuration and therefore do not count as references.
pub fn check_unused_bindings(view: &crate::config_loader::AuthoredBindingView) -> Vec<String> {
    check_unused_bindings_in_file(view.parsed())
}

fn check_unused_bindings_in_file(parsed: &crate::parser::ParsedFile) -> Vec<String> {
    let resources = parsed.iter_all_resources().collect();
    let deferred_sites = parsed.deferred_for_sites().collect();
    check_unused_bindings_in_file_parts(parsed, resources, deferred_sites)
}

#[cfg(test)]
fn check_unused_bindings_in_resolved_file(
    parsed: &crate::parser::ModuleResolvedParsedFile,
) -> Vec<String> {
    let resources = parsed.iter_all_resources().collect();
    let deferred_sites = parsed.deferred_for_sites().collect();
    check_unused_bindings_in_file_parts(parsed, resources, deferred_sites)
}

fn check_unused_bindings_in_file_parts<'a, B, D>(
    parsed: &'a crate::parser::File<
        crate::parser::ParsedExportParam<ParserPlaceholder, B>,
        ParserPlaceholder,
        ParserPlaceholder,
        D,
        B,
    >,
    resources: Vec<
        ResourceRef<'a, crate::resource::ModuleSource, ParserPlaceholder, ParserPlaceholder, D, B>,
    >,
    deferred_sites: Vec<crate::parser::DeferredForSite<'a, ParserPlaceholder, B>>,
) -> Vec<String>
where
    B: crate::resource::BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut referenced: HashSet<B> = HashSet::new();
    let mut scoped_bare_references: HashSet<(crate::parser::BindingScopeId, String)> =
        HashSet::new();
    let mut state_block_targets = Vec::new();
    for state_block in &parsed.state_blocks {
        match state_block {
            crate::parser::StateBlock::Import { to, to_span: _, id } => {
                collect_dependencies(id, &mut referenced);
                state_block_targets.push(to.clone());
            }
            crate::parser::StateBlock::Moved { addresses } => {
                state_block_targets.push(addresses.to());
            }
            crate::parser::StateBlock::Removed { .. } => {}
        }
    }
    // `moved.from` and `removed.from` identify state rows that should no
    // longer be configured. Plan rejects a desired resource at either source
    // address as a collision, so source addresses do not count as binding use.

    // Collect all defined binding names (skip discard pattern `_`).
    // Walk top-level and for-body resources so bindings declared inside a
    // `for` template are also tracked. State-block targets count as uses only
    // for the top-level managed-resource arm, matching plan's `parsed.resources`
    // target-resolution set.
    let mut defined_bindings: Vec<(B, Option<crate::parser::BindingScopeId>)> = Vec::new();
    for rref in &resources {
        if let Some(binding) = rref.binding_address() {
            if binding.as_str() == "_" {
                continue;
            }
            let scope = parsed
                .parser_binding_metadata
                .resource_source_file(rref.id())
                .map(crate::parser::BindingScopeId::root);
            defined_bindings.push((binding.clone(), scope));
            if matches!(rref, ResourceRef::Resource(_))
                && state_block_targets
                    .iter()
                    .any(|target| target.matches_resource_id(rref.id()))
            {
                referenced.insert(binding.clone());
            }
        }
    }

    if defined_bindings.is_empty() {
        return Vec::new();
    }

    // Collect all referenced binding names. Walk both top-level resources
    // and for-body template resources so bindings referenced only inside a
    // `for` loop are counted as used.
    for rref in &resources {
        let attrs = rref.attributes();
        for value in attrs.values() {
            collect_dependencies(value, &mut referenced);
        }
        // `LiveComposition` has no explicit dependencies, so this walk is
        // skipped for that arm.
        for dep in rref.explicit_dependencies().into_iter().flatten() {
            referenced.insert(dep.binding_reference());
        }
    }
    for deferred in deferred_sites {
        collect_dependencies(deferred.iterable, &mut referenced);
    }
    for call in &parsed.module_calls {
        for (_, value) in call.arguments() {
            collect_dependencies(value.walker_value(), &mut referenced);
        }
    }
    // Bare lexical values resolve to their ordinary runtime Value, while a
    // pending structural leaf is deliberately omitted from that Value tree.
    // Their authored names live only in parser side data, so preserve the
    // existing unused-binding semantics by consuming that provenance here.
    scoped_bare_references.extend(
        parsed
            .bare_binding_uses()
            .uses()
            .map(|use_site| (use_site.scope.clone(), use_site.name.clone())),
    );
    for attr_param in &parsed.attribute_params {
        if let Some(expression) = &attr_param.value {
            match expression {
                crate::parser::BoundaryExpression::Value(value) => {
                    collect_dependencies(value, &mut referenced);
                }
                crate::parser::BoundaryExpression::Handle(handle) => {
                    referenced.insert(handle.canonical_binding().clone());
                }
            }
        }
    }
    for export_param in &parsed.export_params {
        if let Some(expression) = &export_param.value {
            match expression {
                crate::parser::BoundaryExpression::Value(value) => {
                    collect_dependencies(value, &mut referenced);
                }
                crate::parser::BoundaryExpression::Handle(handle) => {
                    referenced.insert(handle.canonical_binding().clone());
                }
            }
        }
    }
    // Each `wait <target> { ... }` declaration references its target
    // and every binding in `depends_on = [...]`. The until predicate's
    // LHS is rooted at the target (enforced by parser), so the target
    // covers the LHS path too.
    for wb in &parsed.wait_bindings {
        referenced.insert(wb.target.clone());
        for dep in &wb.depends_on {
            referenced.insert(dep.binding_reference());
        }
    }

    // Return unused binding names, skipping structurally-required bindings
    // (if/for/read expressions) and for-generated indexed bindings (e.g., vpcs[0])
    defined_bindings
        .into_iter()
        .filter(|(binding, scope)| {
            !referenced.contains(binding)
                && !scope.as_ref().is_some_and(|scope| {
                    scoped_bare_references
                        .iter()
                        .any(|(candidate_scope, name)| {
                            candidate_scope == scope && name == binding.as_str()
                        })
                })
                && !parsed.structural_bindings.contains(binding)
                && !binding.as_str().contains('[')
        })
        .map(|(binding, _)| binding.as_str().to_string())
        .collect()
}

/// Check for unused bindings while retaining bindings that provide stable
/// identities for otherwise-conflicting attribute-derived resources.
pub fn check_unused_bindings_with_identity_requirements(
    view: &crate::config_loader::AuthoredBindingView,
    registry: &crate::schema::SchemaRegistry,
) -> Vec<String> {
    let identity_required =
        crate::identifier::identity_required_bindings(&view.parsed().resources, registry);
    check_unused_bindings(view)
        .into_iter()
        .filter(|binding| !identity_required.contains(binding))
        .collect()
}

/// Find unused bindings and their locations in the same authored scope.
/// Expanded module inventories must not supply locations for root names.
pub fn unused_binding_findings(
    view: &crate::config_loader::AuthoredBindingView,
    registry: &crate::schema::SchemaRegistry,
) -> Result<Vec<crate::parser::Located<String>>, crate::parser::AnalysisTreeError> {
    let tree = crate::parser::AnalysisTree::try_from_file(view.parsed())?;
    Ok(locate_unused_bindings(
        &tree,
        check_unused_bindings_with_identity_requirements(view, registry),
    ))
}

/// Attach unused authored names to the declaration inventory. Generated loop
/// members have no separate name declaration and receive no independent warning.
pub fn locate_unused_bindings<D, B: BindingAddress, E>(
    tree: &crate::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B, E>,
    names: impl IntoIterator<Item = String>,
) -> Vec<crate::parser::Located<String>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let names: HashSet<_> = names.into_iter().collect();
    tree.syntax()
        .declarations()
        .iter()
        .filter(|declaration| {
            declaration.kind() == crate::config_loader::DeclarationKind::Binding
                && names.contains(declaration.name())
        })
        .map(|declaration| {
            crate::parser::Located::new(
                declaration.location().clone(),
                declaration.name().to_string(),
            )
        })
        .collect()
}

/// Walk `field_path` against `start`. Return `Ok(tail_type)` on a
/// clean walk and `Err((mismatched_type, bad_segment))` for the first
/// segment that can't be resolved. Lists, maps, and scalars never host
/// `.field` access — the parent type they reach is the right anchor
/// for the diagnostic builder's "use iteration / subscript / nothing"
/// suggestion.
///
/// Walk `start` through a chain of `.field` segments (the leading
/// `PathSegment::Field` prefix of an [`AccessPath`]). Stops at the first
/// non-`Field` segment and returns the type at that position together
/// with the index of the segment that stopped descent — callers that
/// need to continue with subscripts use [`narrow_type_expr`], callers
/// that only care about the field-path leg use [`walk_type_expr_fields`].
///
/// Walks by reference so deep struct paths don't pay an O(depth) clone
/// chain — the caller clones once at the return site if it needs an
/// owned copy.
///
/// `Map` segments unwrap to the value type so dot-form key access
/// (`accounts.k → T`) is symmetric with the subscript form
/// (`accounts['k']`). #2447.
pub(crate) fn walk_type_expr_fields<'a, 'b>(
    start: &'a TypeExpr,
    field_path: &'b [String],
) -> Result<&'a TypeExpr, (&'a TypeExpr, &'b str)> {
    let mut current = start;
    for segment in field_path {
        match current {
            TypeExpr::Struct { fields } => match fields.iter().find(|(name, _)| name == segment) {
                Some((_, ty)) => current = ty,
                None => return Err((current, segment.as_str())),
            },
            TypeExpr::Map(inner) => current = inner.as_ref(),
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::List(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => return Err((current, segment.as_str())),
        }
    }
    Ok(current)
}

/// Narrow `start` through an `AccessPath`'s ordered segments — a free
/// mix of `.field` and `[index]` continuations at any depth
/// (carina#3025). Returns `None` when a step doesn't fit the container
/// kind; those mismatches are reported by the dedicated shape checkers
/// and a duplicate here would be noise.
///
/// Used by both upstream-export type-checking and module-call
/// attribute-export inference.
pub(crate) fn narrow_type_expr(
    start: &TypeExpr,
    segments: &[crate::resource::PathSegment],
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> Option<TypeExpr> {
    use crate::resource::{PathSegment, Subscript};
    let mut current = start.clone();
    for seg in segments {
        current = match current {
            TypeExpr::Struct { fields } => match seg {
                PathSegment::Field { name } => {
                    let (_, ty) = fields.into_iter().find(|(n, _)| n == name)?;
                    ty
                }
                PathSegment::Subscript { .. } => return None,
            },
            TypeExpr::Map(inner) => match seg {
                PathSegment::Field { .. }
                | PathSegment::Subscript {
                    index: Subscript::Str { .. },
                } => *inner,
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                } => return None,
            },
            TypeExpr::Ref(path) | TypeExpr::DottedUnresolved(path) => match seg {
                PathSegment::Field { name } => {
                    let schemas = schemas?;
                    let schema = schemas
                        .get(
                            &path.provider,
                            &path.resource_type,
                            crate::schema::SchemaKind::Resource,
                        )
                        .or_else(|| {
                            schemas.get(
                                &path.provider,
                                &path.resource_type,
                                crate::schema::SchemaKind::DataSource,
                            )
                        })?;
                    let schema = schema.as_schema();
                    let attribute = schema.attributes.get(name)?;
                    inference::attribute_type_to_type_expr(&attribute.attr_type, &path.provider)
                }
                PathSegment::Subscript { .. } => return None,
            },
            TypeExpr::List(inner) => match seg {
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                } => *inner,
                PathSegment::Field { .. }
                | PathSegment::Subscript {
                    index: Subscript::Str { .. },
                } => return None,
            },
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => return None,
        };
    }
    Some(current)
}

pub mod inference;

#[cfg(test)]
mod named_annotation_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod secret_tests;

#[cfg(test)]
mod review15_error_tests {
    use super::*;

    #[test]
    fn round15_resource_reference_preserves_the_typed_schema_failure() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse("let source = test.Source {}\n", &config).unwrap();
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            crate::schema::ResourceSchema::new("Source")
                .attribute(
                    crate::schema::AttributeSchema::provider_populated(
                        "name",
                        crate::schema::AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("test.SourceName")),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let bindings = BindingIndex::from_parsed(&parsed, &schemas, &config);
        let interpolation = lift_type_expr(&INTERPOLATION_REF_SINK).unwrap();
        let context = ResourceReferenceContext {
            location: ReferenceFindingLocation::Attribute("name".into()),
            bindings: &bindings,
            declared_source: None,
            interpolation_sink: &interpolation,
        };
        let sink = AttributeType::refined_string(
            Some(TypeIdentity::from_dotted("test.TargetName")),
            None,
            None,
            None,
        );
        let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
        let reference = crate::resource::AuthoredAccessPath::new("source", "name");
        let finding = check_resource_reference(
            &context,
            ReferencePathRef::Access(&reference),
            Some(RefSink::EffectiveType {
                attr_type: &sink,
                defs: crate::schema::empty_defs_for_schema_walks(),
                ancestry: FieldAncestry::WRITABLE,
                owner: &owner,
            }),
            crate::resource::ResourceRefUsage::Required,
        )
        .expect("the unavailable schema must be reported");
        assert!(
            matches!(finding.kind, ReferenceFindingKind::SchemaUnavailable(SecretDiagnosticError::Public(error)) if matches!(*error, crate::parser::CustomTypeValidationError::ProvidersNotLoaded))
        );
    }
}
