//! Validation utilities for resources and modules

pub mod deferred_populate;
pub mod depends_on;
pub mod typecheck;
pub mod wait;

use std::collections::HashSet;
use std::hash::Hash;

use indexmap::IndexMap;

use crate::binding_index::{BindingIndex, RefType, RefTypeError, ResolvedRefType};
use crate::deps::collect_dependencies;
use crate::parser::{
    ForBinding, LoadedProviderContext, ModuleCall, ModuleCallId, ParsedValue as Value,
    ProviderContext, ResourceContext, ResourceRef, ResourceTypePath, SourceFileId, SourceSpan,
    TypeExpr,
};
use crate::provider::ProviderFactory;
use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, CompositionCallPath, ConcreteValue, DeferredValue,
    EnumValueResolver, ModuleSource, ParserPlaceholder, PlaceholderKind, RawEnumIdentifier,
    ReferencePath, ReferencePathRef, RelativeBindingName, Resource as GenericResource,
    Value as GenericValue,
};
use crate::schema::{
    BindingKind, BindingShadowsEnumMemberError, CompiledAttributeType as AttributeType,
    FieldAncestry, SchemaRegistry, Shape, TypeError, TypeIdentity,
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
/// resource handle satisfy a closed set by accident. Resource annotations
/// (`Ref` and its pre-resolution `DottedUnresolved` form) lift to a distinct,
/// string-backed handle kind. The handle's path is retained for BindingIndex's
/// single declared-type assignability relation; the runtime transport remains
/// an id string until #3803 lands. Unchecked states never enter this function.
pub fn lift_type_expr(type_expr: &TypeExpr) -> Option<AttributeType> {
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
        TypeExpr::Simple(name) => Some(AttributeType::refined_string(
            Some(TypeIdentity::bare(crate::parser::snake_to_pascal(name))),
            None,
            None,
            None,
        )),
        TypeExpr::SchemaType { owned } => Some(AttributeType::refined_string(
            Some(owned.identity().clone()),
            None,
            None,
            None,
        )),
        TypeExpr::List(inner) => lift_type_expr(inner).map(AttributeType::list),
        TypeExpr::Map(inner) => lift_type_expr(inner).map(AttributeType::map),
        TypeExpr::Struct { fields } => fields
            .iter()
            .map(|(name, field_type)| {
                lift_type_expr(field_type)
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
                        | TypeExpr::List(_)
                        | TypeExpr::Map(_)
                        | TypeExpr::Ref(_)
                        | TypeExpr::DottedUnresolved(_)
                        | TypeExpr::SchemaType { .. }
                        | TypeExpr::Struct { .. }
                        | TypeExpr::Union(_) => None,
                    })
                    .collect(),
            ))
        }
        TypeExpr::Union(members) => members
            .iter()
            .map(lift_type_expr)
            .collect::<Option<Vec<_>>>()
            .filter(|members| !members.is_empty())
            .map(AttributeType::union),
        TypeExpr::Ref(path) | TypeExpr::DottedUnresolved(path) => Some(
            AttributeType::resource_handle(path.provider.clone(), path.resource_type.clone()),
        ),
    }
}

static STRING_REF_SINK: TypeExpr = TypeExpr::String;
static INT_REF_SINK: TypeExpr = TypeExpr::Int;

/// The declared type at one position in a [`Value`] tree.
///
/// Module boundaries use `TypeExpr`; provider resource attributes use
/// `AttributeType` plus their definition map. `String` represents the inside
/// of an interpolation, whose expression parts are rendered to text regardless
/// of the outer container.
#[derive(Clone, Copy)]
pub(crate) enum RefSink<'a> {
    TypeExpr(&'a TypeExpr),
    EffectiveType {
        attr_type: &'a AttributeType,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        ancestry: crate::schema::FieldAncestry,
    },
    String,
}

impl<'a> RefSink<'a> {
    fn from_effective_type(effective: crate::schema::EffectiveType<'a>) -> Self {
        Self::EffectiveType {
            attr_type: effective.as_type(),
            defs: effective.defs(),
            ancestry: effective.ancestry(),
        }
    }

    fn as_type_expr(self) -> Option<&'a TypeExpr> {
        match self {
            Self::TypeExpr(type_expr) => Some(type_expr),
            Self::String => Some(&STRING_REF_SINK),
            Self::EffectiveType { .. } => None,
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
                    | TypeExpr::Map(_)
                    | TypeExpr::Ref(_)
                    | TypeExpr::DottedUnresolved(_)
                    | TypeExpr::SchemaType { .. }
                    | TypeExpr::Struct { .. }
                    | TypeExpr::StringLiteral(_)
                    | TypeExpr::Union(_) => None,
                }))
                .map(Self::TypeExpr)
            }
            Self::EffectiveType {
                attr_type,
                defs,
                ancestry,
            } => match attr_type.shape_with_defs(defs) {
                Shape::List {
                    element_type: inner,
                    ..
                } => Some(Self::EffectiveType {
                    attr_type: inner,
                    defs,
                    ancestry,
                }),
                Shape::Union => {
                    unique_union_list_element(attr_type, defs).map(|inner| Self::EffectiveType {
                        attr_type: inner,
                        defs,
                        ancestry,
                    })
                }
                _ => None,
            },
            Self::TypeExpr(
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::Map(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_),
            )
            | Self::String => None,
        }
    }

    fn map_entry(self, key: &str) -> Option<Self> {
        match self {
            Self::TypeExpr(TypeExpr::Map(inner)) => Some(Self::TypeExpr(inner)),
            Self::TypeExpr(TypeExpr::Struct { fields }) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, field_type)| Self::TypeExpr(field_type)),
            Self::TypeExpr(TypeExpr::Union(members)) => exactly_one(
                members
                    .iter()
                    .filter(|member| matches!(member, TypeExpr::Map(_) | TypeExpr::Struct { .. })),
            )
            .and_then(|member| Self::TypeExpr(member).map_entry(key)),
            Self::EffectiveType {
                attr_type,
                defs,
                ancestry,
            } => match attr_type.shape_with_defs(defs) {
                Shape::Map { value: inner, .. } => Some(Self::EffectiveType {
                    attr_type: inner,
                    defs,
                    ancestry,
                }),
                Shape::Struct { .. } => {
                    let fields = crate::schema::struct_fields_with_defs(attr_type, defs)
                        .expect("Shape::Struct must expose struct fields internally");
                    crate::schema::struct_field_for_key(fields, key, ancestry).map(|field| {
                        Self::EffectiveType {
                            attr_type: &field.field_type,
                            defs,
                            ancestry: ancestry.descend(field),
                        }
                    })
                }
                Shape::Union => unique_union_map_member(attr_type, defs).and_then(|member| {
                    Self::EffectiveType {
                        attr_type: member,
                        defs,
                        ancestry,
                    }
                    .map_entry(key)
                }),
                _ => None,
            },
            Self::TypeExpr(
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::List(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::StringLiteral(_),
            )
            | Self::String => None,
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
    expected: String,
    actual: String,
    declared: Option<String>,
    path: ModuleCallDiagnosticPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModuleCallDiagnosticPath {
    Authored(ReferencePath<RelativeBindingName>),
    Expanded(ReferencePath<AbsoluteBindingAddress>),
}

impl ModuleCallDiagnosticPath {
    fn to_dot_string(&self) -> String {
        match self {
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
            Self::Authored(path) => {
                format_module_call_ref_error(formatter, call_label, argument, path, kind)
            }
            Self::Expanded(path) => {
                format_module_call_ref_error(formatter, call_label, argument, path, kind)
            }
        }
    }
}

impl std::fmt::Display for BindingReferenceTypeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Type mismatch: expected {}, got {} ({})",
            self.expected,
            self.actual,
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
}

/// Typed inputs shared by CLI and LSP while consuming parser-authored bare
/// binding uses. Provider context is deliberately mandatory: bootstrap parsing
/// may leave dotted declarations unresolved, but this validation seam may not.
pub struct ValidationTypeContext<'a, B: BindingAddress = RelativeBindingName> {
    pub schemas: &'a SchemaRegistry,
    pub module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    provider_context: typecheck::TypecheckProviderContext<'a>,
    scope_bindings: BindingIndex<'a, B>,
    inference_bindings: inference::InferenceBindings<ParserPlaceholder, B>,
}

impl<'a, B: BindingAddress> ValidationTypeContext<'a, B> {
    pub fn new<E, D>(
        file: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &'a SchemaRegistry,
        provider_context: &'a LoadedProviderContext,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::with_provider_context(
            file,
            schemas,
            typecheck::TypecheckProviderContext::loaded(provider_context),
            module_signatures,
        )
    }

    /// Build the validation view used by bootstrap/editor callers that have
    /// no loaded provider custom-type catalog. Structural checks still run,
    /// while provider-owned leaves never reach the resolver.
    pub fn new_providerless<E, D>(
        file: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &'a SchemaRegistry,
        provider_context: &'a ProviderContext,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::with_provider_context(
            file,
            schemas,
            typecheck::TypecheckProviderContext::providerless(provider_context),
            module_signatures,
        )
    }

    fn with_provider_context<E, D>(
        file: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &'a SchemaRegistry,
        provider_context: typecheck::TypecheckProviderContext<'a>,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self {
            schemas,
            provider_context,
            module_signatures,
            scope_bindings: BindingIndex::from_parsed_with_module_signatures(
                file,
                schemas,
                module_signatures,
            ),
            inference_bindings: inference::bindings_from_parsed(file),
        }
    }

    fn raw_provider_context(&self) -> &'a ProviderContext {
        self.provider_context.as_provider_context()
    }
}

/// One typed failure tied directly to the parser token that authored the use.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingUseFinding {
    pub use_site: crate::parser::BareBindingUse,
    pub error: BindingResolutionError,
}

impl BindingUseFinding {
    fn error(use_site: crate::parser::BareBindingUse, error: BindingResolutionError) -> Self {
        Self { use_site, error }
    }
}

impl std::fmt::Display for BindingUseFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} at line {}",
            self.use_site.source_file.path().display(),
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
}

impl std::fmt::Display for BindingUseReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, finding) in self.findings.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            write!(
                formatter,
                "{}: {} at line {}",
                finding.use_site.source_file.path().display(),
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
    BindingUse(#[from] BindingUseReport),
    #[error(transparent)]
    LeftoverPlaceholders(#[from] crate::resource::LeftoverPlaceholders),
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
    config: &ProviderContext,
    target: &ExpectedEnumTarget<'_>,
    resolver: &EnumValueResolver<'_, crate::schema::Compiled>,
    identity: &TypeIdentity,
) -> Result<(), BindingResolutionError> {
    if let Some(declared_type) = declared_type {
        let resolved = resolve_type_expr(declared_type, config)
            .map_err(BindingResolutionError::TypeResolution)?;
        match resolved {
            TypeExpr::SchemaType { owned }
                if owned.identity().same_type(identity)
                    && owned.identity().assignable_to(identity) =>
            {
                return Ok(());
            }
            other @ (TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::List(_)
            | TypeExpr::Map(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_)) => {
                return Err(BindingResolutionError::Type(TypeError::TypeMismatch {
                    expected: identity.to_string(),
                    got: other.to_string(),
                }));
            }
        }
    }
    match value {
        GenericValue::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
            if !canonical.identity().same_type(identity)
                || !canonical.identity().assignable_to(identity)
            {
                return Err(BindingResolutionError::Type(TypeError::TypeMismatch {
                    expected: identity.to_string(),
                    got: canonical.identity().to_string(),
                }));
            }
            resolver
                .resolve_state_text(canonical.api_value())
                .map(|_| ())
                .map_err(BindingResolutionError::Type)
        }
        GenericValue::Concrete(ConcreteValue::EnumIdentifier(raw)) => resolver
            .resolve_raw(raw)
            .map(|_| ())
            .map_err(BindingResolutionError::Type),
        _ => crate::schema::validate_attr_with_defs_and_ancestry(
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
    target: ExpectedEnumTarget<'_>,
    existing_error: BindingResolutionError,
) -> BindingResolutionError {
    let resolver = EnumValueResolver::with_defs(target.attr_type, target.defs);
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
        } if EnumValueResolver::with_defs(attr_type, defs)
            .enum_identity()
            .is_some() =>
        {
            Some(ExpectedEnumTarget {
                attr_type,
                defs,
                ancestry,
            })
        }
        RefSink::EffectiveType { .. } | RefSink::TypeExpr(_) | RefSink::String => None,
    }
}

fn declared_value_assignable_to_sink(
    name: &str,
    declared_type: &TypeExpr,
    sink: RefSink<'_>,
    config: &ProviderContext,
) -> Result<(), BindingResolutionError> {
    let resolved =
        resolve_type_expr(declared_type, config).map_err(BindingResolutionError::TypeResolution)?;
    let Some(source) = lift_type_expr(&resolved) else {
        return Ok(());
    };
    let source = crate::schema::TypeInSchema::schemaless(&source);
    let string_sink = AttributeType::string();
    let lifted_sink;
    let target = match sink {
        RefSink::EffectiveType {
            attr_type, defs, ..
        } => attr_type.in_schema(defs),
        RefSink::String => crate::schema::TypeInSchema::schemaless(&string_sink),
        RefSink::TypeExpr(type_expr) => {
            let Some(lifted) = lift_type_expr(type_expr) else {
                return Ok(());
            };
            lifted_sink = lifted;
            crate::schema::TypeInSchema::schemaless(&lifted_sink)
        }
    };
    if source.is_assignable_to(target) {
        return Ok(());
    }
    Err(BindingResolutionError::Existing(format!(
        "Type mismatch: expected {}, got {} (from {name})",
        target.resolved_type_name(),
        source.resolved_type_name(),
    )))
}

fn reference_value_mismatch<B: BindingAddress>(
    reference: ReferencePathRef<'_, B>,
    target: crate::schema::TypeInSchema<'_, crate::schema::Compiled>,
    bindings: &BindingIndex<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Option<BindingResolutionError> {
    match bindings.ref_type(reference) {
        RefType::Typed(source) => source.is_assignable_to_sink(target).err().map(|mismatch| {
            BindingResolutionError::ReferenceType(Box::new(BindingReferenceTypeError {
                expected: target.resolved_type_name(),
                actual: mismatch.actual_type_name().to_string(),
                declared: mismatch.declared_type_name().map(str::to_string),
                path: diagnostic_path(reference),
            }))
        }),
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
    value: &GenericValue<ParserPlaceholder, B>,
    target: crate::schema::TypeInSchema<'_, crate::schema::Compiled>,
    inference_bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    bindings: &BindingIndex<'_, B>,
    authored_binding: &str,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Option<BindingResolutionError> {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values()[1..].iter().find_map(|branch| {
                deferred_value_mismatch(
                    branch,
                    target,
                    inference_bindings,
                    schemas,
                    bindings,
                    authored_binding,
                    diagnostic_path,
                )
            })
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { path }) => reference_value_mismatch(
            ReferencePathRef::Access(path),
            target,
            bindings,
            diagnostic_path,
        ),
        GenericValue::Deferred(DeferredValue::BindingRef { binding }) => reference_value_mismatch(
            ReferencePathRef::Binding(binding),
            target,
            bindings,
            diagnostic_path,
        ),
        GenericValue::Deferred(
            DeferredValue::Coalesce { .. }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => check_deferred_access_type(value, target, inference_bindings, schemas, bindings)
            .err()
            .map(|error| match error {
                DeferredAccessTypeError::TypeMismatch {
                    expected,
                    actual,
                    origin: _origin,
                } => {
                    // Inference describes a parser-internal unresolved-module
                    // placeholder here. The authored binding path is the
                    // stable source used for the shared diagnostic and dedupe.
                    BindingResolutionError::ReferenceType(Box::new(BindingReferenceTypeError {
                        expected,
                        actual,
                        declared: None,
                        path: ModuleCallDiagnosticPath::Authored(ReferencePath::Binding(
                            RelativeBindingName::new(authored_binding.to_string()),
                        )),
                    }))
                }
                DeferredAccessTypeError::InvalidAccess { error } => {
                    BindingResolutionError::DeferredAccess(Box::new(
                        DeferredAccessTypeError::InvalidAccess { error },
                    ))
                }
            }),
        GenericValue::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::List(_)
            | ConcreteValue::StringList(_)
            | ConcreteValue::Map(_),
        ) => None,
    }
}

fn precheck_value_source<B: BindingAddress>(
    use_site: &crate::parser::BareBindingUse,
    value: &GenericValue<ParserPlaceholder, B>,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Option<BindingResolutionError> {
    let string_sink = AttributeType::string();
    let lifted_sink;
    let target = match sink {
        RefSink::EffectiveType {
            attr_type, defs, ..
        } => attr_type.in_schema(defs),
        RefSink::String => crate::schema::TypeInSchema::schemaless(&string_sink),
        RefSink::TypeExpr(type_expr) => {
            let lifted = lift_type_expr(type_expr)?;
            lifted_sink = lifted;
            crate::schema::TypeInSchema::schemaless(&lifted_sink)
        }
    };
    deferred_value_mismatch(
        value,
        target,
        &context.inference_bindings,
        context.schemas,
        &context.scope_bindings,
        &use_site.name,
        diagnostic_path,
    )
}

fn resolve_value_source_against_sink<B: BindingAddress>(
    use_site: &crate::parser::BareBindingUse,
    value: &GenericValue<ParserPlaceholder, B>,
    declared_type: Option<&TypeExpr>,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_, B>,
    diagnostic_path: &impl Fn(ReferencePathRef<'_, B>) -> ModuleCallDiagnosticPath,
) -> Result<(), BindingResolutionError> {
    let prechecked = declared_type
        .is_none()
        .then(|| precheck_value_source(use_site, value, sink, context, diagnostic_path))
        .flatten();

    if let Some(target) = enum_target_from_sink(sink) {
        let resolver = EnumValueResolver::with_defs(target.attr_type, target.defs);
        let identity = resolver
            .enum_identity()
            .expect("enum_target_from_sink proved the enum identity");
        let existing = match prechecked {
            Some(error) => Err(error),
            None => binding_value_assignable_to_enum(
                value,
                declared_type,
                context.raw_provider_context(),
                &target,
                &resolver,
                &identity,
            ),
        };
        return existing.map_err(|error| enum_shadow_or_existing(use_site, target, error));
    }

    if let Some(error) = prechecked {
        return Err(error);
    }
    if let Some(declared_type) = declared_type {
        return declared_value_assignable_to_sink(
            &use_site.name,
            declared_type,
            sink,
            context.raw_provider_context(),
        );
    }

    match sink {
        RefSink::EffectiveType {
            attr_type,
            defs,
            ancestry,
        } => crate::schema::validate_attr_with_defs_and_ancestry(attr_type, value, defs, ancestry)
            .map_err(BindingResolutionError::Type),
        RefSink::String => crate::schema::validate_attr_with_defs_and_ancestry(
            &AttributeType::string(),
            value,
            crate::schema::empty_defs_for_schema_walks(),
            FieldAncestry::WRITABLE,
        )
        .map_err(BindingResolutionError::Type),
        RefSink::TypeExpr(type_expr) => context
            .provider_context
            .check(type_expr, value, typecheck::TypecheckInference::Unavailable)
            .map_err(|error| BindingResolutionError::ValueType(Box::new(error))),
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
        Some(ResourceRef::DataSource(data_source)) => {
            schemas.get_for_data_source(data_source)?.schema()
        }
        Some(ResourceRef::Resource(resource)) => schemas.get_for(resource)?.schema(),
        Some(ResourceRef::Deferred {
            resource,
            deferred: _deferred,
        }) => schemas.get_for(resource.as_resource())?.schema(),
        None => return None,
    };
    let mut segments = path.segments().iter();
    let crate::parser::AttributePathSegment::Attribute(root) = segments.next()? else {
        return None;
    };
    let mut sink = RefSink::from_effective_type(
        schema
            .effective_attribute_by_authored_name(root)?
            .value_type(),
    );
    for segment in segments {
        sink = match segment {
            crate::parser::AttributePathSegment::ListIndex(_) => sink.list_element()?,
            crate::parser::AttributePathSegment::Key(key)
            | crate::parser::AttributePathSegment::StructField(key) => sink.map_entry(key)?,
            crate::parser::AttributePathSegment::Attribute(_) => return None,
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
            | AttributePathSegment::StructField(_) => None,
        },
        TypeExpr::Map(element) => match segment {
            AttributePathSegment::Key(_) | AttributePathSegment::StructField(_) => {
                Some((**element).clone())
            }
            AttributePathSegment::Attribute(_) | AttributePathSegment::ListIndex(_) => None,
        },
        TypeExpr::Struct { fields } => match segment {
            AttributePathSegment::Key(name) | AttributePathSegment::StructField(name) => fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, field_type)| field_type.clone()),
            AttributePathSegment::Attribute(_) | AttributePathSegment::ListIndex(_) => None,
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
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { .. }
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
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::Struct { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Union(_)) => ResolvedParameterType::Value(value),
    }))
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

    match &record.source {
        crate::parser::BareBindingSource::Value {
            value,
            declared_type,
        } => {
            if let Some(declaration) = enum_declaration.as_ref() {
                let target = ExpectedEnumTarget {
                    attr_type: declaration,
                    defs: crate::schema::empty_defs_for_schema_walks(),
                    ancestry: FieldAncestry::WRITABLE,
                };
                let resolver = EnumValueResolver::with_defs(target.attr_type, target.defs);
                let identity = resolver
                    .enum_identity()
                    .expect("module enum declaration must have an enum identity");
                return binding_value_assignable_to_enum(
                    value,
                    declared_type.as_deref(),
                    context.raw_provider_context(),
                    &target,
                    &resolver,
                    &identity,
                )
                .map_err(|error| enum_shadow_or_existing(&record.authored, target, error));
            }
            resolve_value_source_against_sink(
                &record.authored,
                value,
                declared_type.as_deref(),
                RefSink::TypeExpr(&expected),
                context,
                diagnostic_path,
            )
        }
        crate::parser::BareBindingSource::Structural(handle) => {
            let existing = match check_handle_sink(handle, &resolved) {
                Ok(()) => return Ok(()),
                Err(error) => BindingResolutionError::Existing(error.to_string()),
            };
            match enum_declaration.as_ref() {
                Some(declaration) => Err(enum_shadow_or_existing(
                    &record.authored,
                    ExpectedEnumTarget {
                        attr_type: declaration,
                        defs: crate::schema::empty_defs_for_schema_walks(),
                        ancestry: FieldAncestry::WRITABLE,
                    },
                    existing,
                )),
                None => Err(existing),
            }
        }
    }
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
        ) && (use_site.path.segments().len() == 1
            || report
                .findings()
                .iter()
                .any(|finding| same_authored_use(&finding.use_site, use_site)))
    })
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
    };

    for record in file.bare_binding_uses().records() {
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
                            let existing =
                                BindingResolutionError::Existing(handle.bare_error().to_string());
                            match enum_target_from_sink(sink) {
                                Some(target) => {
                                    Err(enum_shadow_or_existing(&record.authored, target, existing))
                                }
                                None => Err(existing),
                            }
                        }
                    },
                    None => match &record.source {
                        crate::parser::BareBindingSource::Value { .. } => Ok(()),
                        crate::parser::BareBindingSource::Structural(handle) => Err(
                            BindingResolutionError::Existing(handle.bare_error().to_string()),
                        ),
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
                    crate::parser::BareBindingSource::Structural(handle) => Err(
                        BindingResolutionError::Existing(handle.bare_error().to_string()),
                    ),
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
                handle.bare_error().to_string(),
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

/// Validate parser-authored bare binding uses in their relative source scope.
pub fn check_bare_binding_uses<
    E,
    D: crate::parser::DeferredForPhaseTraversal<
            ParserPlaceholder,
            ParserPlaceholder,
            RelativeBindingName,
        >,
>(
    file: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, RelativeBindingName>,
    context: &ValidationTypeContext<'_, RelativeBindingName>,
) -> BindingUseReport {
    check_bare_binding_uses_for_phase(file, context, |path| {
        ModuleCallDiagnosticPath::Authored(path.to_owned())
    })
}

/// Validate expanded bare binding uses without projecting absolute addresses
/// back into an authored scope.
pub fn check_expanded_bare_binding_uses<
    E,
    D: crate::parser::DeferredForPhaseTraversal<
            ParserPlaceholder,
            ParserPlaceholder,
            AbsoluteBindingAddress,
        >,
>(
    file: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, AbsoluteBindingAddress>,
    context: &ValidationTypeContext<'_, AbsoluteBindingAddress>,
) -> BindingUseReport {
    check_bare_binding_uses_for_phase(file, context, |path| {
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
    let report = {
        let context =
            ValidationTypeContext::new(&file, schemas, provider_context, module_signatures);
        check_expanded_bare_binding_uses(&file, &context)
    };
    if !report.is_ready() {
        return Err(report.into());
    }

    Ok(ResolvedFile {
        inner: file.into_expanded()?,
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
/// enclosing sink and interpolation expressions use a string sink.
pub(crate) fn visit_refs_with_sink<'a, P: PlaceholderKind, B: BindingAddress>(
    value: &GenericValue<P, B>,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(
        ReferencePathRef<'_, B>,
        Option<RefSink<'a>>,
        crate::resource::ResourceRefUsage,
        Option<&GenericValue<P, B>>,
    ),
) {
    visit_refs_with_sink_inner(value, sink, crate::resource::ResourceRefUsage::Required, f);
}

fn visit_refs_with_sink_inner<'a, P: PlaceholderKind, B: BindingAddress>(
    value: &GenericValue<P, B>,
    sink: Option<RefSink<'a>>,
    usage: crate::resource::ResourceRefUsage,
    f: &mut impl FnMut(
        ReferencePathRef<'_, B>,
        Option<RefSink<'a>>,
        crate::resource::ResourceRefUsage,
        Option<&GenericValue<P, B>>,
    ),
) {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let [condition, then_value, else_value] = deferred.values();
            visit_refs_with_sink_inner(condition, None, usage, f);
            for branch in [then_value, else_value] {
                visit_refs_with_sink_inner(branch, sink, usage, f);
            }
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { path }) => {
            f(ReferencePathRef::<B>::Access(path), sink, usage, None)
        }
        GenericValue::Deferred(DeferredValue::BindingRef { binding }) => {
            f(ReferencePathRef::<B>::Binding(binding), sink, usage, None)
        }
        GenericValue::Deferred(DeferredValue::Placeholder(placeholder)) => {
            if let Some(name) = placeholder.module_argument_name()
                && let Some(binding) =
                    crate::resource::module_argument_placeholder::<B>(name.to_string())
            {
                f(ReferencePathRef::<B>::Binding(&binding), sink, usage, None);
            }
        }
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                visit_refs_with_sink_inner(item, child_sink, usage, f);
            }
        }
        GenericValue::Concrete(ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                let child_sink = sink.and_then(|sink| sink.map_entry(key));
                visit_refs_with_sink_inner(value, child_sink, usage, f);
            }
        }
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    visit_refs_with_sink_inner(value, Some(RefSink::String), usage, f);
                }
            }
        }
        GenericValue::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            // The enclosing sink constrains the function result, not its
            // inputs. Function parameter sites are walked separately with
            // BindingIndex's signature authority.
            for argument in args {
                visit_refs_with_sink_inner(argument.walker_value(), None, usage, f);
            }
        }
        GenericValue::Deferred(DeferredValue::Access(access)) => {
            if let Some(reference) = crate::resource::module_argument_reference_path(value) {
                // Module parameters are typed value placeholders rather than
                // structural references. Project their postfix chain through
                // BindingIndex so `cfg.field` receives the same narrowing and
                // diagnostic origin as an ordinary path.
                f(reference.as_ref(), sink, usage, None);
            } else {
                // Other Access values have computed operands. References
                // nested inside the computation do not inherit its result
                // sink.
                visit_refs_with_sink_inner(access.base(), None, usage, f);
            }
        }
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            visit_refs_with_sink_inner(inner, sink, usage, f)
        }
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            let value_usage = if value.is_coalesce_optional_operand() {
                crate::resource::ResourceRefUsage::CoalesceValue
            } else {
                crate::resource::ResourceRefUsage::Required
            };
            let default_usage = if usage == crate::resource::ResourceRefUsage::CoalesceValue
                && default.is_coalesce_optional_operand()
            {
                crate::resource::ResourceRefUsage::CoalesceValue
            } else {
                crate::resource::ResourceRefUsage::Required
            };
            visit_refs_with_sink_inner(value, sink, value_usage, f);
            visit_refs_with_sink_inner(default, sink, default_usage, f);
        }
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let operand_sink = Some(RefSink::TypeExpr(&TypeExpr::Int));
            visit_refs_with_sink_inner(expression.lhs(), operand_sink, usage, f);
            visit_refs_with_sink_inner(expression.rhs(), operand_sink, usage, f);
        }
        GenericValue::Concrete(ConcreteValue::String(_))
        | GenericValue::Concrete(ConcreteValue::EnumIdentifier(_))
        | GenericValue::Concrete(ConcreteValue::CanonicalEnum(_))
        | GenericValue::Concrete(ConcreteValue::Int(_))
        | GenericValue::Concrete(ConcreteValue::Float(_))
        | GenericValue::Concrete(ConcreteValue::Bool(_))
        | GenericValue::Concrete(ConcreteValue::Duration(_))
        | GenericValue::Concrete(ConcreteValue::StringList(_))
        | GenericValue::Deferred(DeferredValue::RedactedSecret { .. })
        | GenericValue::Deferred(DeferredValue::Unknown(_)) => {}
    }
}

/// Visit values whose postfix path needs inference in addition to the normal
/// reference walk. Computed Access values need their result checked; a
/// canonical ResourceRef with path segments also needs inference so a known
/// scalar cannot be indexed merely because BindingIndex reserves shape
/// diagnostics for upstream references.
fn visit_computed_accesses_with_sink<'a, B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(&GenericValue<ParserPlaceholder, B>, Option<RefSink<'a>>),
) {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let [condition, then_value, else_value] = deferred.values();
            visit_computed_accesses_with_sink(condition, None, f);
            for branch in [then_value, else_value] {
                visit_computed_accesses_with_sink(branch, sink, f);
            }
        }
        GenericValue::Deferred(DeferredValue::Access(access)) => {
            if crate::resource::module_argument_reference_path(value).is_none() {
                f(value, sink);
                visit_computed_accesses_with_sink(access.base(), None, f);
            }
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { path })
            if !path.segments().is_empty() =>
        {
            f(value, sink);
        }
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                visit_computed_accesses_with_sink(item, child_sink, f);
            }
        }
        GenericValue::Concrete(ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                visit_computed_accesses_with_sink(
                    value,
                    sink.and_then(|sink| sink.map_entry(key)),
                    f,
                );
            }
        }
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    visit_computed_accesses_with_sink(value, Some(RefSink::String), f);
                }
            }
        }
        GenericValue::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for argument in args {
                visit_computed_accesses_with_sink(argument.walker_value(), None, f);
            }
        }
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            visit_computed_accesses_with_sink(inner, sink, f)
        }
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            visit_computed_accesses_with_sink(value, sink, f);
            visit_computed_accesses_with_sink(default, sink, f);
        }
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            // Interpolation renders the final concrete Int to text; it does
            // not turn the arithmetic node into a String expression. Keep an
            // Int result sink at that boundary while still honoring ordinary
            // resource/module sinks everywhere else.
            let result_sink = match sink {
                Some(RefSink::String) => Some(RefSink::TypeExpr(&INT_REF_SINK)),
                other => other,
            };
            f(value, result_sink);
            let operand_sink = Some(RefSink::TypeExpr(&INT_REF_SINK));
            visit_computed_accesses_with_sink(expression.lhs(), operand_sink, f);
            visit_computed_accesses_with_sink(expression.rhs(), operand_sink, f);
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { .. })
        | GenericValue::Deferred(DeferredValue::BindingRef { .. })
        | GenericValue::Concrete(ConcreteValue::String(_))
        | GenericValue::Concrete(ConcreteValue::EnumIdentifier(_))
        | GenericValue::Concrete(ConcreteValue::CanonicalEnum(_))
        | GenericValue::Concrete(ConcreteValue::Int(_))
        | GenericValue::Concrete(ConcreteValue::Float(_))
        | GenericValue::Concrete(ConcreteValue::Bool(_))
        | GenericValue::Concrete(ConcreteValue::Duration(_))
        | GenericValue::Concrete(ConcreteValue::StringList(_))
        | GenericValue::Deferred(DeferredValue::RedactedSecret { .. })
        | GenericValue::Deferred(DeferredValue::Unknown(_))
        | GenericValue::Deferred(DeferredValue::Placeholder(_)) => {}
    }
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
        expected: String,
        actual: String,
        origin: String,
    },
}

impl std::fmt::Display for DeferredAccessTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAccess { error } => error.fmt(f),
            Self::TypeMismatch {
                expected,
                actual,
                origin,
            } => write!(
                f,
                "Type mismatch: expected {expected}, got {actual} ({origin})"
            ),
        }
    }
}

impl DeferredAccessTypeError {
    /// Return the authored operator span when this computed-expression error
    /// came from integer arithmetic. Frontends use this only for source
    /// anchoring; the diagnostic text and validity decision remain in core.
    pub const fn integer_operator_span(
        &self,
    ) -> Option<crate::integer_arithmetic::IntegerOperatorSpan> {
        match self {
            Self::InvalidAccess {
                error:
                    inference::InferenceError::InvalidIntegerArithmeticOperand { operator_span, .. },
            } => Some(*operator_span),
            Self::InvalidAccess { .. } | Self::TypeMismatch { .. } => None,
        }
    }
}

/// One arithmetic-specific failure found while walking a computed value.
/// The error text and validity decision come from the shared core checker;
/// frontends only use the span to anchor that finding in authored source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerArithmeticTypeFinding {
    error: DeferredAccessTypeError,
    operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
}

impl IntegerArithmeticTypeFinding {
    pub fn error(&self) -> &DeferredAccessTypeError {
        &self.error
    }

    pub const fn operator_span(&self) -> crate::integer_arithmetic::IntegerOperatorSpan {
        self.operator_span
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
    expected: crate::schema::TypeInSchema<'_, crate::schema::Compiled>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    // Canonical reference-rooted postfix expressions are ResourceRefs. Route
    // those through BindingIndex so managed resources and wait aliases share
    // the same schema/value-source authority. This decision belongs here,
    // rather than in every CLI/LSP caller, so a new binding kind cannot be
    // silently skipped by a copied `InferenceBinding` predicate.
    if let GenericValue::Deferred(DeferredValue::ResourceRef { path }) = value {
        return match binding_index.postfix_ref_type(path) {
            RefType::Typed(source) => match source.is_assignable_to_sink(expected) {
                Ok(()) => Ok(()),
                Err(mismatch) => Err(DeferredAccessTypeError::TypeMismatch {
                    expected: expected.resolved_type_name(),
                    actual: mismatch.actual_type_name().to_string(),
                    origin: mismatch.origin_description(path.into()),
                }),
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

    let actual = match inference::infer_type_from_value(value, bindings, schemas) {
        Ok(actual) => actual,
        Err(crate::static_type::InferenceIssue::Failed(
            error @ inference::InferenceError::InvalidAccess { .. },
        ))
        | Err(crate::static_type::InferenceIssue::Failed(
            error @ inference::InferenceError::UnknownAttribute { .. },
        )) => {
            return Err(DeferredAccessTypeError::InvalidAccess { error });
        }
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::IncompatibleCoalesceTypes { left, default },
        )) => {
            return Err(DeferredAccessTypeError::TypeMismatch {
                expected: left,
                actual: default,
                origin: "from coalesce default".to_string(),
            });
        }
        Err(crate::static_type::InferenceIssue::Failed(
            error @ inference::InferenceError::InvalidIntegerArithmeticOperand { .. },
        )) => return Err(DeferredAccessTypeError::InvalidAccess { error }),
        // Dynamic result types and unavailable authorities cannot be checked
        // soundly at this phase. Spell every inference variant out so adding a
        // new failure reason forces an explicit checked/unchecked decision.
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::UnknownType { .. }
            | inference::InferenceError::HeterogeneousCollection { .. }
            | inference::InferenceError::UnknownBinding { .. }
            | inference::InferenceError::CyclicVirtualBinding { .. },
        ))
        | Err(crate::static_type::InferenceIssue::NotInferable(
            crate::static_type::NotInferableReason::MissingValue
            | crate::static_type::NotInferableReason::NonInferableBinding { .. }
            | crate::static_type::NotInferableReason::SchemaUnavailable { .. },
        )) => return Ok(()),
    };
    let Some(actual) = lift_type_expr(&actual) else {
        return Ok(());
    };
    let actual = crate::schema::TypeInSchema::schemaless(&actual);
    if actual.is_assignable_to(expected) {
        return Ok(());
    }
    Err(DeferredAccessTypeError::TypeMismatch {
        expected: expected.resolved_type_name(),
        actual: actual.resolved_type_name(),
        origin: format!("from {}", crate::value::format_value(value)),
    })
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

fn prove_integer_operand<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    expression: &crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
    side: crate::integer_arithmetic::IntegerOperandSide,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Result<(), DeferredAccessTypeError> {
    let integer = AttributeType::int();
    let integer_sink = crate::schema::TypeInSchema::schemaless(&integer);
    let reference_type = match value {
        GenericValue::Deferred(DeferredValue::ResourceRef { path }) => {
            Some(binding_index.postfix_ref_type(path))
        }
        GenericValue::Deferred(DeferredValue::BindingRef { binding }) => {
            Some(binding_index.ref_type(ReferencePathRef::Binding(binding)))
        }
        GenericValue::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::UpstreamExport { path },
        )) => Some(binding_index.postfix_ref_type(path)),
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            return prove_integer_operand(
                inner,
                expression,
                side,
                bindings,
                schemas,
                binding_index,
            );
        }
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            prove_integer_operand(value, expression, side, bindings, schemas, binding_index)?;
            return prove_integer_operand(
                default,
                expression,
                side,
                bindings,
                schemas,
                binding_index,
            );
        }
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            prove_integer_operand(
                deferred.then_value(),
                expression,
                side,
                bindings,
                schemas,
                binding_index,
            )?;
            return prove_integer_operand(
                deferred.else_value(),
                expression,
                side,
                bindings,
                schemas,
                binding_index,
            );
        }
        // The nested node has an intrinsic Int result; its own callback in the
        // same walk proves its operands and owns any inner diagnostic span.
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(_)) => return Ok(()),
        GenericValue::Deferred(DeferredValue::Placeholder(placeholder)) => placeholder
            .module_argument_name()
            .and_then(|name| crate::resource::module_argument_placeholder::<B>(name.to_string()))
            .map(|binding| binding_index.ref_type(ReferencePathRef::Binding(&binding))),
        GenericValue::Concrete(_)
        | GenericValue::Deferred(
            DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => None,
    };

    if let Some(RefType::Typed(source)) = reference_type {
        return source
            .is_assignable_to_sink(integer_sink)
            .map_err(|mismatch| {
                invalid_integer_operand(expression, side, mismatch.actual_type_name().to_string())
            });
    }

    match inference::infer_type_from_value(value, bindings, schemas) {
        Ok(TypeExpr::Int) => Ok(()),
        Ok(actual) => Err(invalid_integer_operand(
            expression,
            side,
            actual.to_string(),
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
) -> Result<(), DeferredAccessTypeError> {
    let integer = AttributeType::int();
    let actual = crate::schema::TypeInSchema::schemaless(&integer);
    let expected = match sink {
        Some(RefSink::EffectiveType {
            attr_type, defs, ..
        }) => attr_type.in_schema(defs),
        Some(RefSink::TypeExpr(type_expr)) => {
            let Some(expected) = lift_type_expr(type_expr) else {
                return Err(DeferredAccessTypeError::TypeMismatch {
                    expected: type_expr.to_string(),
                    actual: "Int".to_string(),
                    origin: format!("from {}", crate::value::format_value(value)),
                });
            };
            if actual.is_assignable_to(crate::schema::TypeInSchema::schemaless(&expected)) {
                return Ok(());
            }
            return Err(DeferredAccessTypeError::TypeMismatch {
                expected: crate::schema::TypeInSchema::schemaless(&expected).resolved_type_name(),
                actual: "Int".to_string(),
                origin: format!("from {}", crate::value::format_value(value)),
            });
        }
        // Interpolation is the sole textual boundary. The arithmetic node
        // itself still has an Int result, and no external sink means its own
        // intrinsic Int contract is sufficient.
        Some(RefSink::String) | None => return Ok(()),
    };
    if actual.is_assignable_to(expected) {
        Ok(())
    } else {
        Err(DeferredAccessTypeError::TypeMismatch {
            expected: expected.resolved_type_name(),
            actual: "Int".to_string(),
            origin: format!("from {}", crate::value::format_value(value)),
        })
    }
}

fn check_integer_arithmetic_types_with_sink<'a, B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'a>>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Vec<IntegerArithmeticTypeFinding> {
    let mut findings = Vec::new();
    visit_computed_accesses_with_sink(value, sink, &mut |candidate, candidate_sink| {
        let GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) = candidate else {
            return;
        };
        let result = prove_integer_operand(
            expression.lhs(),
            expression,
            crate::integer_arithmetic::IntegerOperandSide::Left,
            bindings,
            schemas,
            binding_index,
        )
        .and_then(|()| {
            prove_integer_operand(
                expression.rhs(),
                expression,
                crate::integer_arithmetic::IntegerOperandSide::Right,
                bindings,
                schemas,
                binding_index,
            )
        })
        .and_then(|()| check_integer_result_sink(candidate, candidate_sink));
        let Err(error) = result else {
            return;
        };
        let operator_span = error
            .integer_operator_span()
            .unwrap_or_else(|| expression.operator_span());
        let duplicate = findings
            .iter()
            .any(|finding: &IntegerArithmeticTypeFinding| {
                finding.to_string() == error.to_string()
                    && finding.operator_span.line() == operator_span.line()
                    && finding.operator_span.column() == operator_span.column()
            });
        if !duplicate {
            findings.push(IntegerArithmeticTypeFinding {
                error,
                operator_span,
            });
        }
    });
    findings
}

/// Validate every integer-arithmetic node in a value tree against both its
/// authored resource sink and its invariant Int operand sinks. This is the
/// shared source for CLI and LSP arithmetic diagnostics; callers add only
/// their surrounding resource context and source-range conversion.
pub fn check_integer_arithmetic_types<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    sink: crate::schema::EffectiveType<'_>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Vec<IntegerArithmeticTypeFinding> {
    check_integer_arithmetic_types_with_sink(
        value,
        Some(RefSink::from_effective_type(sink)),
        bindings,
        schemas,
        binding_index,
    )
}

/// Validate arithmetic in a value that has no external declared sink, such as
/// a top-level `let`. The expression's own result is Int, and every nested
/// arithmetic node must independently prove both operands are Int.
pub fn check_integer_arithmetic_value<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Vec<IntegerArithmeticTypeFinding> {
    check_integer_arithmetic_types_with_sink(
        value,
        Some(RefSink::TypeExpr(&INT_REF_SINK)),
        bindings,
        schemas,
        binding_index,
    )
}

fn append_unique_integer_arithmetic_findings<B: BindingAddress>(
    findings: &mut Vec<IntegerArithmeticTypeFinding>,
    value: &GenericValue<ParserPlaceholder, B>,
    sink: Option<RefSink<'_>>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) {
    for finding in
        check_integer_arithmetic_types_with_sink(value, sink, bindings, schemas, binding_index)
    {
        let span = finding.operator_span();
        let duplicate = findings.iter().any(|existing| {
            let existing_span = existing.operator_span();
            existing.to_string() == finding.to_string()
                && existing_span.line() == span.line()
                && existing_span.column() == span.column()
        });
        if !duplicate {
            findings.push(finding);
        }
    }
}

/// Validate arithmetic in every non-resource value declaration in a parsed
/// file. Resource attributes retain their external schema sink and are checked
/// separately by [`check_integer_arithmetic_types`]; this pass covers values
/// that may otherwise be unused, including module attributes, provider/backend
/// values, deferred iterables, and user-function bodies.
pub fn check_file_integer_arithmetic_values<
    B: BindingAddress,
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_, B>,
) -> Vec<IntegerArithmeticTypeFinding> {
    let mut findings = Vec::new();
    let mut check =
        |value: &GenericValue<ParserPlaceholder, B>,
         sink: Option<RefSink<'_>>,
         value_bindings: &inference::InferenceBindings<ParserPlaceholder, B>| {
            append_unique_integer_arithmetic_findings(
                &mut findings,
                value,
                sink,
                value_bindings,
                schemas,
                binding_index,
            );
        };

    for value in parsed.variables.values() {
        check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)), bindings);
    }
    for call in &parsed.module_calls {
        for (_, argument) in call.arguments() {
            check(
                argument.walker_value(),
                Some(RefSink::TypeExpr(&INT_REF_SINK)),
                bindings,
            );
        }
    }
    for argument in &parsed.arguments {
        if let Some(value) = &argument.default {
            check(
                value,
                Some(RefSink::TypeExpr(&argument.static_type)),
                bindings,
            );
        }
    }
    for attribute in &parsed.attribute_params {
        if let Some(value) = &attribute.value {
            check(
                value,
                attribute.static_type.as_ref().map(RefSink::TypeExpr),
                bindings,
            );
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value() {
            check(
                value,
                export.type_expr_opt().map(RefSink::TypeExpr),
                bindings,
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
            check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)), bindings);
        }
    }
    if let Some(backend) = &parsed.backend {
        for value in backend.attributes.values() {
            check(value, Some(RefSink::TypeExpr(&INT_REF_SINK)), bindings);
        }
    }
    for state_block in &parsed.state_blocks {
        if let crate::parser::StateBlock::Import { id, .. } = state_block {
            check(id, Some(RefSink::TypeExpr(&INT_REF_SINK)), bindings);
        }
    }
    // Wait arithmetic is fully evaluated while parsing; the typed RHS is a
    // placeholder-free literal and therefore has no deferred arithmetic node
    // for this parser-stage proof walk.
    for deferred in parsed.deferred_for_sites() {
        check(
            deferred.iterable,
            Some(RefSink::TypeExpr(&INT_REF_SINK)),
            bindings,
        );
    }

    for function in parsed.user_functions.values() {
        let mut function_bindings = bindings.clone();
        for parameter in &function.params {
            // A same-named directory binding must not accidentally prove an
            // unannotated lexical parameter.
            function_bindings.remove(&parameter.name);
            if let Some(type_expr) = &parameter.param_type {
                function_bindings.insert(
                    parameter.name.clone(),
                    inference::InferenceBinding::Argument {
                        type_expr: type_expr.clone(),
                    },
                );
            }
        }
        for parameter in &function.params {
            if let Some(value) = &parameter.default {
                check(
                    value,
                    parameter.param_type.as_ref().map(RefSink::TypeExpr),
                    &function_bindings,
                );
            }
        }
        for (name, value, _) in &function.local_lets {
            function_bindings.remove(name);
            check(
                value,
                Some(RefSink::TypeExpr(&INT_REF_SINK)),
                &function_bindings,
            );
            if let Ok(type_expr) =
                inference::infer_type_from_value(value, &function_bindings, schemas)
            {
                function_bindings.insert(
                    name.clone(),
                    inference::InferenceBinding::Argument { type_expr },
                );
            }
        }
        check(
            &function.body.value,
            function.return_type.as_ref().map(RefSink::TypeExpr),
            &function_bindings,
        );
    }

    findings
}

fn resolve_resource_reference<B: BindingAddress>(
    resource_id: &crate::resource::ResourceId,
    reference: ReferencePathRef<'_, B>,
    bindings: &BindingIndex<'_, B>,
    usage: crate::resource::ResourceRefUsage,
    all_errors: &mut Vec<String>,
) -> Option<ResolvedRefType> {
    match bindings.ref_type(reference) {
        RefType::Typed(resolved) => Some(resolved),
        RefType::Unchecked => None,
        RefType::UnknownBinding { .. } => {
            all_errors.push(format!(
                "{}: unknown binding '{}' in reference {}",
                resource_id,
                reference.binding(),
                reference.to_dot_string(),
            ));
            None
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {
            None
        }
        RefType::UnknownAttribute(error) => {
            all_errors.push(format!("{}: {}", resource_id, error));
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleCallRefErrorKind {
    TypeMismatch {
        expected: String,
        actual: String,
        declared: Option<String>,
    },
    UnknownAttribute(RefTypeError),
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
    path: &ReferencePath<B>,
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
        ModuleCallRefErrorKind::TypeMismatch {
            expected,
            actual,
            declared,
        } => write!(
            f,
            "module call '{}': argument '{}': cannot assign {} to '{}': expected {}, got {} ({})",
            call_label,
            argument,
            actual,
            argument,
            expected,
            actual,
            match declared {
                Some(declared) => format!("from {path}, declared {declared}"),
                None => format!("from {path}"),
            },
        ),
        ModuleCallRefErrorKind::UnknownAttribute(error) => {
            write!(f, "module call '{}': {}", call_label, error)
        }
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
    )
}

fn validate_module_call_argument_ref_types_inner(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    bindings: &BindingIndex<'_>,
    unknown_policy: UnknownReferencePolicy,
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

            let mut reference_position = 0;
            visit_refs_with_sink(
                value.walker_value(),
                declared_sink.map(RefSink::TypeExpr),
                &mut |path, sink, usage, _candidate| {
                    let current_position = reference_position;
                    reference_position += 1;
                    let Some(kind) =
                        composition_ref_error_kind(path, sink, bindings, unknown_policy, usage)
                    else {
                        return;
                    };
                    errors.push(ModuleCallRefError {
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
        }
    }

    errors
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeParamRefError<B: BindingAddress = RelativeBindingName> {
    pub attribute: String,
    pub path: ReferencePath<B>,
    pub kind: ModuleCallRefErrorKind,
}

impl<B: BindingAddress> std::fmt::Display for AttributeParamRefError<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ModuleCallRefErrorKind::TypeMismatch {
                expected,
                actual,
                declared,
            } => write!(
                f,
                "attribute '{}': type mismatch: expected {}, got {} ({})",
                self.attribute,
                expected,
                actual,
                match declared {
                    Some(declared) =>
                        format!("from {}, declared {}", self.path.to_dot_string(), declared),
                    None => format!("from {}", self.path.to_dot_string()),
                },
            ),
            ModuleCallRefErrorKind::UnknownAttribute(error) => {
                write!(f, "attribute '{}': {}", self.attribute, error)
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
    pub path: ReferencePath<AbsoluteBindingAddress>,
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
    pub module_directory: Option<std::path::PathBuf>,
    pub call_paths: Vec<CompositionCallPath>,
    reference_position: usize,
}

impl CompositionAttributeRefError {
    fn module_label(&self) -> &str {
        self.module_source.as_deref().unwrap_or(&self.module_name)
    }

    fn module_identity(&self) -> String {
        self.module_directory
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| self.module_source.clone())
            .unwrap_or_else(|| self.module_name.clone())
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
    TypeMismatch {
        expected: String,
        actual: String,
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
            ModuleCallRefErrorKind::TypeMismatch {
                expected,
                actual,
                declared,
            } => Self::TypeMismatch {
                expected: expected.clone(),
                actual: actual.clone(),
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
struct CompositionAttributeErrorKey {
    module_identity: String,
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
            let sink = sink
                .and_then(RefSink::as_type_expr)
                .and_then(lift_type_expr)?;
            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
            source
                .is_assignable_to_sink(sink_type)
                .err()
                .map(|mismatch| ModuleCallRefErrorKind::TypeMismatch {
                    expected: sink_type.resolved_type_name(),
                    actual: mismatch.actual_type_name().to_string(),
                    declared: mismatch.declared_type_name().map(str::to_string),
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

fn validate_user_function_ref_types_inner<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D>,
    bindings: &BindingIndex<'_>,
    unknown_policy: UnknownReferencePolicy,
) -> Result<(), String> {
    fn check_refs(
        value: &Value,
        sink: Option<RefSink<'_>>,
        context: &str,
        bindings: &BindingIndex<'_>,
        unknown_policy: UnknownReferencePolicy,
        errors: &mut Vec<String>,
    ) {
        visit_refs_with_sink(value, sink, &mut |path, sink, usage, _candidate| {
            let Some(sink) = sink else {
                return;
            };
            let Some(kind) =
                composition_ref_error_kind(path, Some(sink), bindings, unknown_policy, usage)
            else {
                return;
            };
            let message = match kind {
                ModuleCallRefErrorKind::TypeMismatch {
                    expected,
                    actual,
                    declared,
                } => format!(
                    "{context}: reference {}: expected {expected}, got {actual} ({})",
                    path.to_dot_string(),
                    declared.map_or_else(
                        || format!("from {}", path.to_dot_string()),
                        |declared| format!("from {}, declared {declared}", path.to_dot_string()),
                    )
                ),
                ModuleCallRefErrorKind::UnknownAttribute(error) => {
                    format!("{context}: {error}")
                }
            };
            errors.push(message);
        });
    }

    fn declared_result_mismatch(source: &TypeExpr, sink: &TypeExpr) -> Option<(String, String)> {
        let source = lift_type_expr(source)?;
        let sink = lift_type_expr(sink)?;
        let source = crate::schema::TypeInSchema::schemaless(&source);
        let sink = crate::schema::TypeInSchema::schemaless(&sink);
        (!source.is_assignable_to(sink))
            .then(|| (sink.resolved_type_name(), source.resolved_type_name()))
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
            | TypeExpr::List(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_) => None,
        }
    }

    fn check_function_calls(
        value: &Value,
        result_sink: Option<&TypeExpr>,
        context: Option<&str>,
        bindings: &BindingIndex<'_>,
        unknown_policy: UnknownReferencePolicy,
        errors: &mut Vec<String>,
    ) {
        fn check_call(
            name: &str,
            args: &[crate::resource::FunctionArgument<ParserPlaceholder, RelativeBindingName>],
            function: Option<&crate::parser::UserFunction<RelativeBindingName>>,
            result_context: Option<(&TypeExpr, &str)>,
            bindings: &BindingIndex<'_>,
            unknown_policy: UnknownReferencePolicy,
            errors: &mut Vec<String>,
        ) {
            if let (Some(source), Some((sink, context))) = (
                function.and_then(|function| function.return_type.as_ref()),
                result_context,
            ) && let Some((expected, actual)) = declared_result_mismatch(source, sink)
            {
                errors.push(format!(
                    "{context}: expected {expected}, got {actual} (from function '{name}' return type)"
                ));
            }
            for (index, argument) in args.iter().enumerate() {
                let parameter_sink = function
                    .and_then(|function| function.params.get(index))
                    .and_then(|parameter| parameter.param_type.as_ref());
                let argument_context = format!("function '{name}': argument {}", index + 1);
                let awaits_module_substitution = argument.pending_parameter().is_some();
                if let Some(sink) = parameter_sink
                    && !awaits_module_substitution
                {
                    check_refs(
                        argument.walker_value(),
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
            Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
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
            Value::Deferred(
                DeferredValue::FunctionCall { name, args, .. }
                | DeferredValue::Unavailable { name, args, .. },
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
            Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
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
            Value::Concrete(ConcreteValue::List(items)) => {
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
            Value::Concrete(ConcreteValue::Map(entries)) => {
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
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                for part in parts {
                    if let crate::resource::InterpolationPart::Expr(value) = part {
                        check_function_calls(
                            value,
                            Some(&STRING_REF_SINK),
                            context,
                            bindings,
                            unknown_policy,
                            errors,
                        );
                    }
                }
            }
            Value::Deferred(DeferredValue::Secret(inner)) => check_function_calls(
                inner,
                result_sink,
                context,
                bindings,
                unknown_policy,
                errors,
            ),
            Value::Deferred(DeferredValue::Access(access)) => check_function_calls(
                access.base(),
                None,
                context,
                bindings,
                unknown_policy,
                errors,
            ),
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
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
            Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
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
            Value::Concrete(ConcreteValue::String(_))
            | Value::Concrete(ConcreteValue::EnumIdentifier(_))
            | Value::Concrete(ConcreteValue::CanonicalEnum(_))
            | Value::Concrete(ConcreteValue::Int(_))
            | Value::Concrete(ConcreteValue::Float(_))
            | Value::Concrete(ConcreteValue::Bool(_))
            | Value::Concrete(ConcreteValue::Duration(_))
            | Value::Concrete(ConcreteValue::StringList(_))
            | Value::Deferred(DeferredValue::ResourceRef { .. })
            | Value::Deferred(DeferredValue::BindingRef { .. })
            | Value::Deferred(DeferredValue::RedactedSecret { .. })
            | Value::Deferred(DeferredValue::Unknown(_))
            | Value::Deferred(DeferredValue::Placeholder(_)) => {}
        }
    }

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
        if let Some(value) = &parameter.value {
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
        Err(errors.join("\n"))
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
            let mut reference_position = 0;
            let declared_sink = declared_type.known().map(RefSink::TypeExpr);
            visit_refs_with_sink(
                argument.value(),
                declared_sink,
                &mut |path, sink, usage, _candidate| {
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
                    errors.push(CompositionRefError::ModuleCall(
                        CompositionModuleCallRefError {
                            argument: argument_name.clone(),
                            origin: Box::new(origin),
                            path: path.to_owned(),
                            kind,
                            call_path: call_path.clone(),
                            reference_position: current_position,
                        },
                    ));
                },
            );
        }

        for (attribute_name, attribute) in &composition.signature.attributes {
            if attribute.source_argument().is_some() {
                // The module-local declaration is checked against the
                // argument's declared type before expansion. Re-checking the
                // substituted caller value here duplicates call-boundary
                // errors and incorrectly blames the module output.
                continue;
            }
            let value = attribute.to_value();
            let mut reference_position = 0;
            let declared_sink = attribute
                .declared_type()
                .and_then(|static_type| static_type.known())
                .map(RefSink::TypeExpr);
            visit_refs_with_sink(
                &value,
                declared_sink,
                &mut |path, sink, usage, _candidate| {
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
                    errors.push(CompositionRefError::Attribute(
                        CompositionAttributeRefError {
                            error: AttributeParamRefError {
                                attribute: attribute_name.clone(),
                                path: path.to_owned(),
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

struct ResourceValidationContext<'a> {
    registry: &'a SchemaRegistry,
    known_providers: &'a HashSet<String>,
    provider_context: &'a ProviderContext,
    all_errors: &'a mut Vec<String>,
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
            let is_string_literal = |attr: &str| quoted_string_attrs.contains(attr);
            if let Err(errors) = schema.validate_with_resolver(
                &crate::resource::attrs_to_hashmap(attributes),
                &is_string_literal,
                context.provider_context.custom_type_resolver(),
            ) {
                context
                    .all_errors
                    .extend(errors.into_iter().map(|error| format!("{id}: {error}")));
            }
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
) -> Result<(), String> {
    let mut all_errors = Vec::new();
    // A standalone module receives its providers from the caller, so schema
    // availability cannot be decided until its resources are expanded into a
    // root configuration. Keep every other schema/type check below active for
    // modules; only defer this provider-availability diagnostic.
    if !is_module_definition(parsed) {
        all_errors.extend(
            group_anonymous_resource_schema_errors(parsed.iter_all_resources().filter_map(
                |resource| match resource {
                    ResourceRef::Resource(resource) => {
                        anonymous_resource_schema_key(resource, registry, known_providers)
                    }
                    ResourceRef::Deferred { resource, .. } => anonymous_resource_schema_key(
                        resource.as_resource(),
                        registry,
                        known_providers,
                    ),
                    ResourceRef::LiveComposition(_) | ResourceRef::DataSource(_) => None,
                },
            ))
            .into_iter()
            .map(|error| error.to_string()),
        );
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
    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that resource references have compatible types.
///
/// For example, if `ipv4_ipam_pool_id` expects `IpamPoolId` type,
/// a reference like `vpc.vpc_id` (which is `AwsResourceId`) should be an error.
pub fn validate_resource_ref_types<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    registry: &SchemaRegistry,
    _argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_, B>,
) -> Result<(), String> {
    validate_resource_ref_types_inner(parsed, registry, bindings)
}

/// Recover the lexical type of a bare value that became an entire reference
/// attribute during expansion. The runtime reference stays absolute; only its
/// module-body type source comes from the declaration checked before expansion.
fn module_resource_ref_declaration<'a, E, D, B: BindingAddress>(
    parsed: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    resource_id: &crate::resource::ResourceId,
    attribute: &str,
    value: &GenericValue<ParserPlaceholder, B>,
) -> Option<(&'a str, ResolvedRefType)> {
    let GenericValue::Deferred(DeferredValue::ResourceRef { path: _path }) = value else {
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
                let source = ResolvedRefType::from_value_declaration(declared_type.as_deref()?)?;
                Some((record.authored.name.as_str(), source))
            }
            crate::parser::BareBindingSource::Structural(_handle) => None,
        }
    })
}

fn validate_resource_ref_types_inner<
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    registry: &SchemaRegistry,
    bindings: &BindingIndex<'_, B>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();
    let inference_bindings = inference::bindings_from_parsed(parsed);

    // Validate declarations directly instead of relying on a later resource
    // attribute to happen to expose an expression. Keep the authored
    // operator keys so a let substituted into an attribute is not reported a
    // second time by the schema-sink pass below.
    let mut declaration_arithmetic_findings = HashSet::new();
    for finding in
        check_file_integer_arithmetic_values(parsed, &inference_bindings, registry, bindings)
    {
        let span = finding.operator_span();
        let message = finding.to_string();
        declaration_arithmetic_findings.insert((span.line(), span.column(), message.clone()));
        all_errors.push(message);
    }

    let mut validate_resource =
        |resource_id: &crate::resource::ResourceId,
         resource_location: String,
         attrs: &indexmap::IndexMap<String, GenericValue<ParserPlaceholder, B>>,
         validated_schema: crate::schema::OwnerBoundResourceSchema<'_>,
         module_source: Option<&crate::resource::ModuleSource>| {
            for (attr_name, attr_value) in attrs {
                let declared_source = module_source
                    .and_then(crate::resource::ModuleSource::module_call)
                    .and_then(|_call| {
                        module_resource_ref_declaration(parsed, resource_id, attr_name, attr_value)
                    });
                let effective_sink = validated_schema
                    .effective_attribute_by_authored_name(attr_name)
                    .map(|attribute| attribute.value_type());
                let sink = effective_sink.clone().map(RefSink::from_effective_type);
                let string_sink = AttributeType::string();
                visit_refs_with_sink(
                    attr_value,
                    sink,
                    &mut |reference, sink, usage, _candidate_value| {
                        let Some(source) = resolve_resource_reference(
                            resource_id,
                            reference,
                            bindings,
                            usage,
                            &mut all_errors,
                        ) else {
                            return;
                        };
                        let source = declared_source
                            .as_ref()
                            .map(|(_name, declared)| declared)
                            .unwrap_or(&source);
                        if matches!(
                            reference,
                            ReferencePathRef::Access(path) if !path.segments().is_empty()
                        ) {
                            // The shared postfix authority below owns both narrowing
                            // and sink compatibility for a segmented ResourceRef.
                            return;
                        }
                        let Some(sink_type) = (match sink {
                            Some(RefSink::EffectiveType {
                                attr_type, defs, ..
                            }) => Some(attr_type.in_schema(defs)),
                            Some(RefSink::String) => {
                                Some(crate::schema::TypeInSchema::schemaless(&string_sink))
                            }
                            Some(RefSink::TypeExpr(_)) | None => None,
                        }) else {
                            return;
                        };
                        let Err(mismatch) = source.is_assignable_to_sink(sink_type) else {
                            return;
                        };
                        let source_name = mismatch.actual_type_name();
                        let origin = mismatch.origin_description(reference);
                        all_errors.push(format!(
                            "{}: cannot assign {} to '{}': expected {}, got {} ({})",
                            resource_location,
                            source_name,
                            attr_name,
                            sink_type.resolved_type_name(),
                            source_name,
                            origin,
                        ));
                    },
                );
                visit_computed_accesses_with_sink(attr_value, sink, &mut |access, sink| {
                    if matches!(
                        access,
                        GenericValue::Deferred(DeferredValue::IntegerArithmetic(_))
                    ) {
                        // The shared arithmetic walk below owns these nodes so CLI
                        // and LSP receive the same error selection and span.
                        return;
                    }
                    let sink_type = match sink {
                        Some(RefSink::EffectiveType {
                            attr_type, defs, ..
                        }) => attr_type.in_schema(defs),
                        Some(RefSink::String) => {
                            crate::schema::TypeInSchema::schemaless(&string_sink)
                        }
                        Some(RefSink::TypeExpr(_)) | None => return,
                    };
                    let checked = match declared_source.as_ref() {
                        Some((name, declared)) => declared
                            .is_assignable_to_sink(sink_type)
                            .map_err(|mismatch| DeferredAccessTypeError::TypeMismatch {
                                expected: sink_type.resolved_type_name(),
                                actual: mismatch.actual_type_name().to_string(),
                                origin: format!("from {name}"),
                            }),
                        None => check_deferred_access_type(
                            access,
                            sink_type,
                            &inference_bindings,
                            registry,
                            bindings,
                        ),
                    };
                    match checked {
                        Ok(()) => {}
                        Err(DeferredAccessTypeError::InvalidAccess { error }) => {
                            all_errors.push(format!("{}: {}", resource_location, error));
                        }
                        Err(DeferredAccessTypeError::TypeMismatch {
                            expected,
                            actual,
                            origin,
                        }) => {
                            all_errors.push(format!(
                                "{}: cannot assign {} to '{}': expected {}, got {} ({})",
                                resource_location, actual, attr_name, expected, actual, origin,
                            ));
                        }
                    }
                });
                if let Some(effective_sink) = effective_sink {
                    for finding in check_integer_arithmetic_types(
                        attr_value,
                        effective_sink,
                        &inference_bindings,
                        registry,
                        bindings,
                    ) {
                        let span = finding.operator_span();
                        if declaration_arithmetic_findings.contains(&(
                            span.line(),
                            span.column(),
                            finding.to_string(),
                        )) {
                            continue;
                        }
                        match finding.error() {
                            DeferredAccessTypeError::InvalidAccess { .. } => {
                                all_errors.push(format!("{}: {}", resource_location, finding));
                            }
                            DeferredAccessTypeError::TypeMismatch {
                                expected,
                                actual,
                                origin,
                            } => {
                                all_errors.push(format!(
                                    "{}: cannot assign {} to '{}': expected {}, got {} ({})",
                                    resource_location, actual, attr_name, expected, actual, origin,
                                ));
                            }
                        }
                    }
                }
            }
        };

    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed.
        let (schema, module_source) = match rref {
            ResourceRef::LiveComposition(_) => continue,
            ResourceRef::DataSource(d) => {
                (registry.get_for_data_source(d), d.module_source.as_ref())
            }
            ResourceRef::Resource(m) => (registry.get_for(m), m.module_source.as_ref()),
            ResourceRef::Deferred {
                resource: m,
                deferred: _deferred,
            } => (registry.get_for(m.as_resource()), m.module_source()),
        };
        let Some(validated_schema) = schema else {
            continue;
        };
        let resource_id = rref.id();
        let resource_location = match rref.context() {
            ResourceContext::Direct => resource_id.to_string(),
            ResourceContext::Deferred(deferred) => {
                format!(
                    "for-body `{}` {}",
                    D::deferred_header(deferred),
                    resource_id
                )
            }
        };

        let attrs = rref.attributes();
        validate_resource(
            resource_id,
            resource_location,
            attrs.as_ref(),
            validated_schema,
            module_source,
        );
    }

    all_errors.extend(
        validate_deferred_for_iterable_types_with_bindings(parsed, registry, &inference_bindings)
            .into_iter()
            .map(|diagnostic| diagnostic.to_string()),
    );

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
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

#[cfg(test)]
fn deferred_iterable_shape(
    value: &Value,
    bindings: &inference::InferenceBindings<ParserPlaceholder>,
    schemas: &SchemaRegistry,
) -> Result<DeferredIterableShape, crate::static_type::InferenceIssue> {
    deferred_iterable_shape_for_phase(value, bindings, schemas)
}

fn deferred_iterable_shape_for_phase<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
) -> Result<DeferredIterableShape, crate::static_type::InferenceIssue> {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(_)) => {
            infer_type_from_value_to_iterable_shape(value, bindings, schemas)
        }
        GenericValue::Concrete(ConcreteValue::List(_items)) => Ok(DeferredIterableShape::List),
        GenericValue::Concrete(ConcreteValue::Map(_entries)) => Ok(DeferredIterableShape::Map),
        GenericValue::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::StringList(_),
        )
        | GenericValue::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => infer_type_from_value_to_iterable_shape(value, bindings, schemas),
    }
}

fn infer_type_from_value_to_iterable_shape<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &SchemaRegistry,
) -> Result<DeferredIterableShape, crate::static_type::InferenceIssue> {
    inference::infer_type_from_value(value, bindings, schemas).map(|inferred| match inferred {
        TypeExpr::List(_element) => DeferredIterableShape::List,
        TypeExpr::Map(_element) => DeferredIterableShape::Map,
        non_iterable @ (TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::Struct { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Union(_)) => DeferredIterableShape::NonIterable(non_iterable),
    })
}

/// Static type failure for an iterable retained as a
/// [`crate::parser::DeferredForExpression`].
///
/// The file/line identity lets the LSP reuse the exact CLI validator while
/// anchoring only diagnostics owned by the current editor buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredForIterableTypeDiagnostic {
    pub file: Option<String>,
    pub line: usize,
    pub expression: String,
    pub message: String,
}

impl std::fmt::Display for DeferredForIterableTypeDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "deferred-for iterable `{}` at line {}: {}",
            self.expression, self.line, self.message
        )
    }
}

/// Validate every deferred `for` iterable against its binding pattern.
///
/// Coalesce operands are checked independently: even while the left remains
/// unresolved, a statically known scalar fallback cannot be allowed to reach
/// apply for a loop that requires a list or map.
pub fn validate_deferred_for_iterable_types<E>(
    parsed: &crate::parser::File<E>,
    schemas: &SchemaRegistry,
) -> Vec<DeferredForIterableTypeDiagnostic> {
    let bindings = inference::bindings_from_parsed(parsed);
    validate_deferred_for_iterable_types_with_bindings(parsed, schemas, &bindings)
}

fn validate_deferred_for_iterable_types_with_bindings<
    E,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &SchemaRegistry,
    bindings: &inference::InferenceBindings<ParserPlaceholder, B>,
) -> Vec<DeferredForIterableTypeDiagnostic> {
    let mut diagnostics = Vec::new();
    for deferred in parsed.deferred_for_sites() {
        let expected = match deferred.binding {
            ForBinding::Simple(_) | ForBinding::Indexed(_, _) => DeferredIterableKind::List,
            ForBinding::Map(_, _) => DeferredIterableKind::Map,
        };
        validate_deferred_iterable_operand(
            deferred.iterable,
            DeferredIterableValidationContext {
                expected,
                bindings,
                schemas,
                file: deferred.file,
                line: deferred.line,
                expression: deferred.iterable,
            },
            &mut diagnostics,
        );
    }
    diagnostics
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
    bindings: &'a inference::InferenceBindings<ParserPlaceholder, B>,
    schemas: &'a SchemaRegistry,
    file: Option<&'a str>,
    line: usize,
    expression: &'a GenericValue<ParserPlaceholder, B>,
}

impl<B: BindingAddress> Copy for DeferredIterableValidationContext<'_, B> {}

impl<B: BindingAddress> Clone for DeferredIterableValidationContext<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

fn validate_deferred_iterable_operand<B: BindingAddress>(
    value: &GenericValue<ParserPlaceholder, B>,
    context: DeferredIterableValidationContext<'_, B>,
    diagnostics: &mut Vec<DeferredForIterableTypeDiagnostic>,
) {
    match value {
        GenericValue::Deferred(DeferredValue::Coalesce { value, default }) => {
            validate_deferred_iterable_operand(value, context, diagnostics);
            validate_deferred_iterable_operand(default, context, diagnostics);
            return;
        }
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            validate_deferred_iterable_operand(inner, context, diagnostics);
            return;
        }
        _ => {}
    }

    let inferred = match deferred_iterable_shape_for_phase(value, context.bindings, context.schemas)
    {
        Ok(inferred) => Some(inferred),
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::IncompatibleCoalesceTypes { left, default },
        )) => {
            diagnostics.push(deferred_iterable_diagnostic(
                context,
                format!("coalesce default type `{default}` does not match left type `{left}`"),
            ));
            None
        }
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::HeterogeneousCollection { reason },
        )) => {
            diagnostics.push(deferred_iterable_diagnostic(context, reason));
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
            | crate::static_type::NotInferableReason::NonInferableBinding { .. }
            | crate::static_type::NotInferableReason::SchemaUnavailable { .. },
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
) -> DeferredForIterableTypeDiagnostic {
    DeferredForIterableTypeDiagnostic {
        file: context.file.map(str::to_string),
        line: context.line,
        expression: crate::value::format_value(context.expression),
        message,
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
    validate_attribute_param_ref_types_inner(
        attribute_params,
        bindings,
        UnknownReferencePolicy::Report,
    )
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
            param.value.as_ref(),
            expected_type,
            bindings,
            UnknownReferencePolicy::Report,
            &mut errors,
        );
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
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
            param.value.as_ref(),
            param.static_type.as_ref(),
            bindings,
            unknown_policy,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_attribute_param_refs<P: PlaceholderKind, B: BindingAddress>(
    param_name: &str,
    value: Option<&GenericValue<P, B>>,
    expected_type: Option<&TypeExpr>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    errors: &mut Vec<String>,
) {
    let Some(value) = value else {
        return;
    };
    visit_refs_with_sink(
        value,
        expected_type.map(RefSink::TypeExpr),
        &mut |path, sink, usage, _candidate| {
            check_attribute_param_ref(
                param_name,
                sink.and_then(RefSink::as_type_expr),
                path,
                bindings,
                unknown_policy,
                usage,
                errors,
            );
        },
    );
}

fn check_attribute_param_ref<B: BindingAddress>(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: ReferencePathRef<'_, B>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    usage: crate::resource::ResourceRefUsage,
    errors: &mut Vec<String>,
) {
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let Some(sink) = expected_type.and_then(lift_type_expr) else {
                return;
            };
            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
            if let Err(mismatch) = source.is_assignable_to_sink(sink_type) {
                errors.push(format!(
                    "attribute '{}': type mismatch: expected {}, got {} ({})",
                    param_name,
                    expected_type.expect("lifted expected type exists"),
                    mismatch.actual_type_name(),
                    mismatch.origin_description(path),
                ));
            }
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {}
        RefType::UnknownAttribute(error) => {
            if matches!(unknown_policy, UnknownReferencePolicy::Report) {
                errors.push(format!("attribute '{}': {}", param_name, error));
            }
        }
        RefType::Unchecked | RefType::UnknownBinding { .. } => {}
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
) -> Result<(), String> {
    validate_export_param_ref_types_inner(export_params, bindings, UnknownReferencePolicy::Report)
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
        let Some(value) = param.value() else {
            continue;
        };
        if param.inference_failed() {
            continue;
        }
        let sink = param.type_expr_opt().map(RefSink::TypeExpr);
        visit_refs_with_sink(value, sink, &mut |path, sink, usage, _candidate| {
            check_export_ref(
                param.name(),
                sink.and_then(RefSink::as_type_expr),
                path,
                bindings,
                unknown_policy,
                usage,
                &mut errors,
            );
        });
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_export_ref<B: BindingAddress>(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: ReferencePathRef<'_, B>,
    bindings: &BindingIndex<'_, B>,
    unknown_policy: UnknownReferencePolicy,
    usage: crate::resource::ResourceRefUsage,
    errors: &mut Vec<String>,
) {
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let Some(type_expr) = expected_type else {
                return;
            };
            let Some(sink) = lift_type_expr(type_expr) else {
                return;
            };
            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
            if let Err(mismatch) = source.is_assignable_to_sink(sink_type) {
                let inferred_origin = mismatch
                    .declared_type_name()
                    .map(|_| format!(" ({})", mismatch.origin_description(path)));
                errors.push(format!(
                    "export '{}': type mismatch for '{}': expected {}, got {}{}",
                    param_name,
                    path.to_dot_string(),
                    type_expr,
                    mismatch.actual_type_name(),
                    inferred_origin.as_deref().unwrap_or_default(),
                ));
            }
        }
        RefType::UnknownAttribute(error) if suppress_coalesce_existence_error(&error, usage) => {}
        RefType::UnknownAttribute(error) => {
            if matches!(unknown_policy, UnknownReferencePolicy::Report) {
                errors.push(format!("export '{}': {}", param_name, error));
            }
        }
        RefType::Unchecked | RefType::UnknownBinding { .. } => {}
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
        let owners = crate::parser::NonEmptySchemaTypeOwners::try_from_iter(
            matches
                .iter()
                .flat_map(|owned| owned.owners().iter().cloned()),
        )
        .expect("at least one matching custom type has at least one owner");
        return Ok(TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::with_owners(first.identity().clone(), owners),
        });
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
        TypeExpr::Simple(snake) => Ok(crate::parser::resolve_known_bare_custom_type(snake, config)
            .unwrap_or_else(|| ty.clone())),
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
    handle: &crate::eval_value::StructuralHandle<B>,
    resolved_parameter: &ResolvedParameterType,
) -> Result<(), HandleSinkError> {
    let ResolvedParameterType::Handle(expected) = resolved_parameter else {
        return Err(HandleSinkError {
            message: handle.bare_error().to_string(),
            bare_value_sink: true,
        });
    };

    let actual = match handle.kind() {
        crate::eval_value::StructuralHandleKind::Resource(actual) => actual,
        kind => {
            let binding = handle
                .alias_name()
                .unwrap_or_else(|| handle.canonical_binding_name());
            return Err(HandleSinkError {
                message: format!(
                    "'{binding}' is {}, not a resource of type {expected}",
                    kind.description(),
                ),
                bare_value_sink: false,
            });
        }
    };

    let source =
        AttributeType::resource_handle(actual.provider.clone(), actual.resource_type.clone());
    let sink = lift_type_expr(&TypeExpr::Ref(expected.clone()))
        .expect("TypeExpr::Ref always lifts to a resource handle");
    if source
        .in_schema(crate::schema::empty_defs_for_schema_walks())
        .is_assignable_to(crate::schema::TypeInSchema::schemaless(&sink))
    {
        Ok(())
    } else {
        Err(HandleSinkError {
            message: format!(
                "expects resource type '{expected}', got {}.{}",
                actual.provider, actual.resource_type,
            ),
            bare_value_sink: false,
        })
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
        resolved @ (TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::Struct { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Union(_)) => Ok(ResolvedParameterType::Value(resolved)),
    }
}

/// Resolve a parameter type when the provider catalog is authoritative.
/// Bootstrap parsing deliberately defers an unresolved dotted annotation;
/// the enriched validation pass repeats the check after schemas are loaded.
pub fn resolve_parameter_type_for_check(
    ty: &TypeExpr,
    config: &ProviderContext,
) -> Result<Option<ResolvedParameterType>, StaticTypeResolutionError> {
    match resolve_parameter_type(ty, config) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(_error) if !config.custom_types_loaded() => Ok(None),
        Err(error) => Err(error),
    }
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
    fn unresolved_custom_type(name: String, config: &ProviderContext) -> Self {
        Self::from(crate::parser::unknown_custom_type_error(&name, config))
    }

    fn unresolved_type_name(&self) -> &str {
        match &self.cause {
            StaticTypeResolutionError::UnknownCustomType { name }
            | StaticTypeResolutionError::UnknownCustomTypeWithSuggestion { name, .. } => name,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeResolutionSource {
    source_file: SourceFileId,
    span: SourceSpan,
}

impl TypeResolutionSource {
    fn new(source_file: SourceFileId, span: SourceSpan) -> Self {
        Self { source_file, span }
    }

    pub fn source_file(&self) -> &SourceFileId {
        &self.source_file
    }

    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{context}: {error}")]
pub struct TypeResolutionDiagnostic {
    context: TypeResolutionContext,
    error: TypeResolutionError,
    location: Option<TypeResolutionSource>,
}

impl TypeResolutionDiagnostic {
    fn declaration(
        kind: TypeResolutionDeclarationKind,
        name: String,
        error: TypeResolutionError,
        location: Option<TypeResolutionSource>,
    ) -> Self {
        Self {
            context: TypeResolutionContext::Declaration { kind, name },
            error,
            location,
        }
    }

    fn description(
        description: String,
        error: TypeResolutionError,
        location: Option<TypeResolutionSource>,
    ) -> Self {
        Self {
            context: TypeResolutionContext::Description(description),
            error,
            location,
        }
    }

    pub fn source_location(&self) -> Option<&TypeResolutionSource> {
        self.location.as_ref()
    }

    /// Whether this typed declaration failure supersedes the parser's
    /// location-only backstop for the same unresolved type.
    fn covers_parser_error(
        &self,
        source_file: &std::path::Path,
        parse_error: &crate::parser::ParseError,
    ) -> bool {
        let crate::parser::ParseError::UnresolvedDeclarationType {
            token,
            line: _line,
            message: _message,
        } = parse_error
        else {
            return false;
        };
        self.location.as_ref().is_some_and(|location| {
            location.source_file.path() == source_file
                && location.span.contains(token.span())
                && self.error.unresolved_type_name() == token.name()
        })
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
    config: &ProviderContext,
    diagnostics: impl IntoIterator<Item = &'a TypeResolutionDiagnostic>,
    parser_error: TypeResolutionParserError<'_>,
) -> bool {
    config.custom_types_loaded()
        && diagnostics.into_iter().any(|diagnostic| {
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
    diagnostics: Vec<TypeResolutionDiagnostic>,
    covered_parser_error: bool,
}

impl FileTypeResolutionReport {
    pub fn diagnostics(&self) -> &[TypeResolutionDiagnostic] {
        &self.diagnostics
    }

    pub fn into_diagnostics(self) -> Vec<TypeResolutionDiagnostic> {
        self.diagnostics
    }

    pub fn covered_parser_error(&self) -> bool {
        self.covered_parser_error
    }
}

fn unresolved_bare_type_errors(
    ty: &TypeExpr,
    config: &ProviderContext,
    errors: &mut Vec<TypeResolutionError>,
) {
    match ty {
        TypeExpr::Simple(name)
            if config.custom_types_loaded()
                && !crate::parser::BUILTIN_BARE_CUSTOM_TYPES.contains(&name.as_str()) =>
        {
            errors.push(TypeResolutionError::unresolved_custom_type(
                crate::parser::snake_to_pascal(name),
                config,
            ));
        }
        TypeExpr::List(inner) | TypeExpr::Map(inner) => {
            unresolved_bare_type_errors(inner, config, errors);
        }
        TypeExpr::Union(members) => {
            for member in members {
                unresolved_bare_type_errors(member, config, errors);
            }
        }
        TypeExpr::Struct { fields } => {
            for (name, field_type) in fields {
                let _ = name;
                unresolved_bare_type_errors(field_type, config, errors);
            }
        }
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::SchemaType { owned: _ }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Simple(_) => {}
    }
}

fn resolve_declaration_type(
    ty: &mut TypeExpr,
    config: &ProviderContext,
) -> Vec<TypeResolutionError> {
    match resolve_type_expr(ty, config) {
        Ok(resolved) => {
            *ty = resolved;
            let mut errors = Vec::new();
            unresolved_bare_type_errors(ty, config, &mut errors);
            errors
        }
        Err(error) => vec![error.into()],
    }
}

fn resolve_file_type_exprs_impl<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<TypeResolutionDiagnostic> {
    let mut errors = Vec::new();
    let argument_sources = parsed
        .arguments
        .iter()
        .filter_map(|argument| {
            parsed
                .parser_binding_metadata
                .argument_type_source(&argument.name)
                .map(|(source_file, span)| {
                    (
                        argument.name.clone(),
                        TypeResolutionSource::new(source_file.clone(), span),
                    )
                })
        })
        .collect::<IndexMap<_, _>>();
    for arg in &mut parsed.arguments {
        for error in resolve_declaration_type(&mut arg.static_type, config) {
            errors.push(TypeResolutionDiagnostic::declaration(
                TypeResolutionDeclarationKind::Argument,
                arg.name.clone(),
                error,
                argument_sources.get(&arg.name).cloned(),
            ));
        }
    }
    for ap in &mut parsed.attribute_params {
        let source = TypeResolutionSource::new(
            SourceFileId::new(ap.source_file().to_path_buf()),
            ap.source_span(),
        );
        if let Some(ty) = &mut ap.static_type {
            for error in resolve_declaration_type(ty, config) {
                errors.push(TypeResolutionDiagnostic::declaration(
                    TypeResolutionDeclarationKind::Attribute,
                    ap.name.clone(),
                    error,
                    Some(source.clone()),
                ));
            }
        }
    }
    for ep in &mut parsed.export_params {
        let export_name = ep.name().to_string();
        if let Some(ty) = ep.type_expr_opt_mut() {
            for error in resolve_declaration_type(ty, config) {
                errors.push(TypeResolutionDiagnostic::declaration(
                    TypeResolutionDeclarationKind::Export,
                    export_name.clone(),
                    error,
                    None,
                ));
            }
        }
    }
    for function in parsed.user_functions.values_mut() {
        for parameter in &mut function.params {
            if let Some(ty) = &mut parameter.param_type {
                for error in resolve_declaration_type(ty, config) {
                    errors.push(TypeResolutionDiagnostic::declaration(
                        TypeResolutionDeclarationKind::FunctionParameter,
                        format!("{}' parameter '{}", function.name, parameter.name),
                        error,
                        None,
                    ));
                }
            }
        }
        if let Some(ty) = &mut function.return_type {
            for error in resolve_declaration_type(ty, config) {
                errors.push(TypeResolutionDiagnostic::declaration(
                    TypeResolutionDeclarationKind::FunctionReturn,
                    function.name.clone(),
                    error,
                    None,
                ));
            }
        }
    }
    errors
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
pub fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<TypeResolutionDiagnostic> {
    resolve_file_type_exprs_report(parsed, config, None).into_diagnostics()
}

/// Resolve declarations and decide in core whether a typed finding supersedes
/// an optional parser backstop for the same authored type token.
pub fn resolve_file_type_exprs_report<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
    parser_error: Option<TypeResolutionParserError<'_>>,
) -> FileTypeResolutionReport {
    let diagnostics = resolve_file_type_exprs_impl(parsed, config);
    let covered_parser_error = parser_error.is_some_and(|parser_error| {
        type_resolution_parser_error_is_covered(config, &diagnostics, parser_error)
    });
    FileTypeResolutionReport {
        diagnostics,
        covered_parser_error,
    }
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
        Ok(resolved) => StaticType::Known(ConsumerResolvedType::new(resolved)),
        Err(resolution_error) => StaticType::Unchecked(UnnameableInConsumer::new(
            type_expr.clone(),
            resolution_error,
        )),
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
pub fn resolve_file_type_exprs_best_effort<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
) {
    let _diagnostics = resolve_file_type_exprs_impl(parsed, config);
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
            let resolved = match resolve_type_expr(&type_expr, config) {
                Ok(type_expr) => StaticType::Known(ConsumerResolvedType::new(type_expr)),
                Err(resolution_error) => {
                    StaticType::Unchecked(ConsumerInferredUncheckedReason::UnnameableInConsumer(
                        UnnameableInConsumer::new(type_expr, resolution_error),
                    ))
                }
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

/// Returns `true` if `value` contains any deferred sub-value that the
/// WASM provider boundary would reject (ResourceRef, BindingRef,
/// Interpolation, FunctionCall, Access, Unknown). Used by
/// [`validate_provider_config`] to skip the plugin-side `validate_config`
/// call for attributes whose refs cannot be substituted at validate
/// time. `Secret` is transparent — its inner value is unwrapped because
/// the secret wrapper survives WASM serialization but the inner value
/// must still be checked. carina#3182.
pub fn value_contains_unresolved_ref<P: PlaceholderKind>(value: &GenericValue<P>) -> bool {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(_))
        | GenericValue::Deferred(DeferredValue::IntegerArithmetic(_))
        | GenericValue::Deferred(DeferredValue::ResourceRef { .. })
        | GenericValue::Deferred(DeferredValue::BindingRef { .. })
        | GenericValue::Deferred(DeferredValue::Coalesce { .. })
        | GenericValue::Deferred(DeferredValue::Interpolation(_))
        | GenericValue::Deferred(DeferredValue::FunctionCall { .. })
        | GenericValue::Deferred(DeferredValue::Unavailable { .. })
        | GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { .. })
        | GenericValue::Deferred(DeferredValue::Access(_))
        | GenericValue::Deferred(DeferredValue::RedactedSecret { .. })
        | GenericValue::Deferred(DeferredValue::Unknown(_))
        | GenericValue::Deferred(DeferredValue::Placeholder(_)) => true,
        GenericValue::Deferred(DeferredValue::Secret(inner)) => {
            value_contains_unresolved_ref(inner)
        }
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(value_contains_unresolved_ref)
        }
        GenericValue::Concrete(ConcreteValue::Map(map)) => {
            map.values().any(value_contains_unresolved_ref)
        }
        GenericValue::Concrete(_) => false,
    }
}

/// Validate provider configuration attributes.
///
/// Runs host-side type-level validation using
/// [`ProviderFactory::provider_config_attribute_types`] first, then
/// canonicalizes schema-known enum values before delegating to
/// [`ProviderFactory::validate_config`] for provider-specific semantic checks.
/// The provider sees only canonical API spellings; literal strings are never
/// decoded by textual shape.
///
/// Attributes containing unresolved references (e.g.
/// `assume_role = { role_arn = upstream.arn }` at validate time, before
/// plan/apply has fetched upstream state) are passed through host-side
/// type validation — the config witness's [`crate::schema::EffectiveType`]
/// validator is deferred-aware and no-ops on `Value::Deferred` — but
/// **excluded from the plugin-side
/// `validate_config` call**, because the WASM serializer rejects
/// deferred values. The same `validate_config` runs again at the typed
/// provider-construction boundary once the refs have been substituted by
/// [`resolve_provider_attributes_with_remote`], so no plugin-side check
/// is permanently lost. carina#3182.
pub fn validate_provider_config<E>(
    parsed: &crate::parser::File<E>,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), String> {
    for provider in &parsed.providers {
        let Some(factory) = factories.iter().find(|f| f.name() == provider.name) else {
            continue;
        };
        crate::executor::provider_ready::validate_parser_provider_config(
            factory.as_ref(),
            &provider.name,
            &provider.attributes,
        )
        .map_err(|error| error.to_string())?;
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
    pub source_file: SourceFileId,
    pub span: SourceSpan,
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
}

impl std::fmt::Display for ModuleCallReferenceDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
    ReferenceType(Box<ModuleCallReferenceDiagnostic>),
    TypeResolution(Box<TypeResolutionDiagnostic>),
    AttributeReference(Box<CompositionAttributeRefError>),
    Constraint(Box<crate::module_resolver::ModuleConstraintDiagnostic>),
}

impl ModuleCallFindingMessage {
    #[cfg(test)]
    fn type_resolution_for_test(context: String, error: StaticTypeResolutionError) -> Self {
        Self::TypeResolution(Box::new(TypeResolutionDiagnostic::description(
            context,
            error.into(),
            None,
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
            | Self::TypeResolution(_)
            | Self::AttributeReference(_)
            | Self::Constraint(_) => false,
        }
    }

    pub(crate) fn type_resolution_diagnostic(&self) -> Option<&TypeResolutionDiagnostic> {
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
            Self::ReferenceType(error) => error.fmt(formatter),
            Self::TypeResolution(error) => error.fmt(formatter),
            Self::AttributeReference(error) => error.fmt(formatter),
            Self::Constraint(diagnostic) => diagnostic.fmt(formatter),
        }
    }
}

impl std::error::Error for ModuleCallFindingMessage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TypeResolution(error) => Some(error.as_ref()),
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
        value_span: SourceSpan,
        message: ModuleCallFindingMessage,
        identity: Option<ModuleCallFindingIdentity>,
    ) -> Self {
        Self {
            kind,
            owners: vec![owner],
            anchor: module_argument_value_anchor(call, value_span),
            message,
            identity,
        }
    }

    pub fn try_from_type_resolution(
        diagnostic: TypeResolutionDiagnostic,
    ) -> Result<Self, Box<TypeResolutionDiagnostic>> {
        let Some(location) = diagnostic.source_location().cloned() else {
            return Err(Box::new(diagnostic));
        };
        let anchor = ModuleCallFindingAnchor {
            source_file: location.source_file().clone(),
            span: location.span(),
        };
        let owner = ModuleCallFindingOwnerSource::Authored(anchor.clone());
        Ok(Self {
            kind: ModuleCallFindingKind::TypeResolution,
            owners: vec![ModuleCallFindingOwner {
                root: owner.clone(),
                immediate: owner,
            }],
            anchor,
            message: ModuleCallFindingMessage::TypeResolution(Box::new(diagnostic)),
            identity: None,
        })
    }

    fn reference_type(
        call_label: &str,
        identity: ModuleCallReferenceIdentity,
        owner: ModuleCallFindingOwner,
        value_span: SourceSpan,
    ) -> Self {
        let display_path = identity.path.clone();
        Self::reference_type_with_display_path(
            call_label,
            identity,
            display_path,
            owner,
            value_span,
        )
    }

    fn reference_type_with_display_path(
        call_label: &str,
        identity: ModuleCallReferenceIdentity,
        display_path: ModuleCallDiagnosticPath,
        owner: ModuleCallFindingOwner,
        value_span: SourceSpan,
    ) -> Self {
        let call = identity.call.clone();
        Self::argument_value(
            ModuleCallFindingKind::ReferenceType,
            owner,
            &call,
            value_span,
            ModuleCallFindingMessage::ReferenceType(Box::new(ModuleCallReferenceDiagnostic {
                call_label: call_label.to_string(),
                identity: identity.clone(),
                display_path,
            })),
            Some(ModuleCallFindingIdentity::ReferenceType(identity)),
        )
    }

    fn binding_use(
        call_label: &str,
        call: &ModuleCallId,
        argument: &str,
        value_span: SourceSpan,
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
                return Self::reference_type(
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
                            expected: error.expected.clone(),
                            actual: error.actual.clone(),
                            declared: error.declared.clone(),
                        },
                    },
                    owner,
                    value_span,
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
            value_span,
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

    pub fn anchor(&self) -> &ModuleCallFindingAnchor {
        &self.anchor
    }

    pub fn message(&self) -> &ModuleCallFindingMessage {
        &self.message
    }
}

impl std::fmt::Display for ModuleCallFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} at line {}",
            self.anchor.source_file.path().display(),
            self.anchor.span.start_line(),
        )
    }
}

impl std::error::Error for ModuleCallFinding {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.message {
            ModuleCallFindingMessage::TypeResolution(error) => Some(error.as_ref()),
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
pub struct ModuleCallCheckInput<'a, 'bindings, E, C: ?Sized = LoadedProviderContext> {
    pub authored: &'a crate::parser::File<E>,
    pub semantic: Option<&'a crate::parser::File<E>>,
    pub base_dir: Option<&'a std::path::Path>,
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
    provider_context: typecheck::TypecheckProviderContext<'a>,
    schemas: &'a SchemaRegistry,
    findings: Vec<ModuleCallFinding>,
    active_modules: HashSet<std::path::PathBuf>,
}

struct ModuleFileCheckContext<'a, 'bindings, E> {
    semantic: Option<&'a crate::parser::File<E>>,
    base_dir: Option<&'a std::path::Path>,
    signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    binding_use_report: &'a BindingUseReport,
    bindings: &'a BindingIndex<'bindings>,
    root_owner: Option<&'a ModuleCallFindingOwnerSource>,
    reference_checks: ModuleCallReferenceChecks,
}

struct ModuleBoundaryCheckContext<'a, E> {
    semantic_call: Option<&'a ModuleCall>,
    authority_file: &'a crate::parser::File<E>,
    signature: &'a crate::module_resolver::ResolvedModuleSignature,
    binding_use_report: &'a BindingUseReport,
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
    ) {
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

            if let Some(signature) = signatures.get(&authored_call.module_name) {
                self.check_call(
                    authored_call,
                    ModuleBoundaryCheckContext {
                        semantic_call,
                        authority_file,
                        signature,
                        binding_use_report,
                        owner: owner.clone(),
                    },
                );
            }

            match reference_checks {
                ModuleCallReferenceChecks::Enabled => {
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
                    self.check_nested_module(authority_file, checked_call, base_dir, &owner.root);
                }
                ModuleCallReferenceChecks::Skipped => {}
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
                    self.check_nested_import(import, base_dir, &root);
                }
            }
        }
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
                value_span,
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
                *value_span,
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
        value_span: SourceSpan,
        finding: &BindingUseFinding,
        owner: &ModuleCallFindingOwner,
    ) {
        self.push_finding(ModuleCallFinding::binding_use(
            call_label,
            call,
            argument,
            value_span,
            finding,
            owner.clone(),
        ));
    }

    fn check_call<E>(
        &mut self,
        authored_call: &ModuleCall,
        context: ModuleBoundaryCheckContext<'_, E>,
    ) {
        let ModuleBoundaryCheckContext {
            semantic_call,
            authority_file,
            signature,
            binding_use_report,
            owner,
        } = context;
        let inference_bindings = inference::bindings_from_parsed(authority_file);
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
                    module_call_anchor(authored_call.id(), authored.key_span()),
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
                    let anchor = module_call_anchor(authored_call.id(), authored.key_span());
                    let diagnostic = TypeResolutionDiagnostic::description(
                        format!(
                            "module {} argument '{}' in {} at line {line}",
                            authored_call.module_name,
                            argument,
                            source_file.display(),
                        ),
                        error.into(),
                        Some(TypeResolutionSource::new(
                            anchor.source_file.clone(),
                            anchor.span,
                        )),
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
                        authored.value_span(),
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
            let inference = if semantic_argument.is_some() {
                typecheck::TypecheckInference::Available {
                    bindings: &inference_bindings,
                    schemas: self.schemas,
                }
            } else {
                typecheck::TypecheckInference::Unavailable
            };
            if let Err(error) =
                self.provider_context
                    .check(&resolved_parameter.as_type_expr(), value, inference)
            {
                self.push_finding(ModuleCallFinding::argument_value(
                    ModuleCallFindingKind::ValueType,
                    owner.clone(),
                    authored_call.id(),
                    authored.value_span(),
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
                    module_call_anchor(authored_call.id(), authored_call.source_span()),
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
        for error in validate_module_call_argument_ref_types_with_bindings(
            std::slice::from_ref(reference_call),
            signatures,
            &HashSet::new(),
            bindings,
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
                || reference_call
                    .argument(&error.identity.argument)
                    .is_some_and(|argument| argument.handle().is_some())
            {
                continue;
            }
            let argument_span = authored_call
                .arguments_with_source_spans()
                .find(|argument| argument.name() == error.identity.argument)
                .map(|argument| argument.value_span())
                .unwrap_or(error.argument_span);
            self.push_finding(ModuleCallFinding::reference_type(
                &module_call_label(authored_call),
                error.identity,
                owner.clone(),
                argument_span,
            ));
        }
    }

    fn check_nested_module<E>(
        &mut self,
        file: &crate::parser::File<E>,
        call: &ModuleCall,
        base_dir: Option<&std::path::Path>,
        root_owner: &ModuleCallFindingOwnerSource,
    ) {
        let Some(import) = file
            .uses
            .iter()
            .find(|import| import.alias == call.module_name)
        else {
            return;
        };
        self.check_nested_import(import, base_dir, root_owner);
    }

    fn check_nested_import(
        &mut self,
        import: &crate::parser::UseStatement,
        base_dir: Option<&std::path::Path>,
        root_owner: &ModuleCallFindingOwnerSource,
    ) {
        let Some(base_dir) = base_dir else {
            return;
        };
        let module_dir = base_dir.join(&import.path);
        let guard = std::fs::canonicalize(&module_dir).unwrap_or_else(|_error| module_dir.clone());
        if !self.active_modules.insert(guard.clone()) {
            return;
        }

        if let Some(mut module) = crate::module_resolver::load_module(&module_dir) {
            let resolution_errors =
                resolve_file_type_exprs(&mut module, self.provider_context.as_provider_context());
            let signatures = crate::module_resolver::load_resolved_module_signatures(
                &module,
                &module_dir,
                self.provider_context.as_provider_context(),
            );
            let bindings = BindingIndex::from_parsed_with_module_signatures(
                &module,
                self.schemas,
                &signatures,
            );
            let binding_use_report = check_bare_binding_uses(
                &module,
                &ValidationTypeContext::with_provider_context(
                    &module,
                    self.schemas,
                    self.provider_context,
                    &signatures,
                ),
            );
            for error in resolution_errors {
                let anchor = error
                    .source_location()
                    .map(|source| ModuleCallFindingAnchor {
                        source_file: source.source_file().clone(),
                        span: source.span(),
                    })
                    .unwrap_or_else(|| module_use_anchor(import));
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
            );
        }

        self.active_modules.remove(&guard);
    }

    fn check_expanded<P: PlaceholderKind>(
        &mut self,
        compositions: &[crate::resource::LiveComposition<P>],
        bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
    ) {
        for error in validate_composition_ref_types_with_bindings(compositions, bindings) {
            match error {
                CompositionRefError::ModuleCall(error) => {
                    let root = error.call_path.root();
                    let immediate = error.call_path.immediate();
                    let span = match *error.origin {
                        crate::parser::ArgumentOrigin::Authored {
                            key_span: _key_span,
                            value_span,
                        } => value_span,
                        crate::parser::ArgumentOrigin::Defaulted => immediate.id.call_span(),
                    };
                    let display_path = ModuleCallDiagnosticPath::Expanded(error.path.clone());
                    self.push_finding(ModuleCallFinding::reference_type_with_display_path(
                        &immediate.display_label(),
                        ModuleCallReferenceIdentity {
                            call: immediate.id.clone(),
                            argument: error.argument,
                            path: ModuleCallDiagnosticPath::Expanded(error.path),
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
                        span,
                    ));
                }
                CompositionRefError::Attribute(error) => {
                    let Some(first_path) = error.call_paths.first() else {
                        unreachable!("expanded attribute errors retain their call path")
                    };
                    let first_immediate = first_path.immediate();
                    let anchor =
                        module_call_anchor(&first_immediate.id, first_immediate.id.call_span());
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
                value_span,
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
                self.push_binding_use_finding(
                    &call_label,
                    call,
                    argument,
                    *value_span,
                    finding,
                    &owner,
                );
            }
        }
    }

    fn check_constraints<P: PlaceholderKind>(
        &mut self,
        reports: &crate::module_resolver::ModuleCallConstraintReports,
        compositions: &[crate::resource::LiveComposition<P>],
    ) {
        for report in reports {
            for outcome in &report.outcomes {
                let crate::module_resolver::ResolvedModuleConstraintStatus::Failed {
                    diagnostic,
                    origin,
                } = &outcome.status
                else {
                    continue;
                };
                let argument_value_span = match origin {
                    crate::module_resolver::ResolvedModuleConstraintOrigin::Argument(
                        crate::parser::ArgumentOrigin::Authored {
                            key_span: _key_span,
                            value_span,
                        },
                    ) => Some(*value_span),
                    crate::module_resolver::ResolvedModuleConstraintOrigin::Argument(
                        crate::parser::ArgumentOrigin::Defaulted,
                    )
                    | crate::module_resolver::ResolvedModuleConstraintOrigin::Call => None,
                };
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
                    let message = ModuleCallFindingMessage::Constraint(diagnostic.clone());
                    match argument_value_span {
                        Some(value_span) => self.push_finding(ModuleCallFinding::argument_value(
                            ModuleCallFindingKind::Constraint,
                            owner,
                            &report.source.id,
                            value_span,
                            message,
                            None,
                        )),
                        None => self.push(
                            ModuleCallFindingKind::Constraint,
                            owner,
                            module_call_anchor(&report.source.id, report.source.call_span()),
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

fn module_call_anchor(call: &ModuleCallId, span: SourceSpan) -> ModuleCallFindingAnchor {
    ModuleCallFindingAnchor {
        source_file: call.source_file_id().clone(),
        span,
    }
}

fn module_argument_value_anchor(
    call: &ModuleCallId,
    value_span: SourceSpan,
) -> ModuleCallFindingAnchor {
    module_call_anchor(call, value_span)
}

fn module_use_anchor(import: &crate::parser::UseStatement) -> ModuleCallFindingAnchor {
    ModuleCallFindingAnchor {
        source_file: SourceFileId::new(import.source_file().to_path_buf()),
        span: import.source_span(),
    }
}

fn module_call_label(call: &ModuleCall) -> String {
    crate::resource::CompositionCall::<RelativeBindingName>::display_label_for_module_call(call)
}

fn module_call_value_is_context_free(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => false,
        Value::Concrete(ConcreteValue::String(_string)) => true,
        Value::Concrete(ConcreteValue::CanonicalEnum(_canonical)) => true,
        Value::Concrete(ConcreteValue::Int(_integer)) => true,
        Value::Concrete(ConcreteValue::Float(_float)) => true,
        Value::Concrete(ConcreteValue::Bool(_boolean)) => true,
        Value::Concrete(ConcreteValue::Duration(_duration)) => true,
        Value::Concrete(ConcreteValue::StringList(_strings)) => true,
        Value::Concrete(ConcreteValue::EnumIdentifier(_identifier)) => false,
        Value::Deferred(DeferredValue::ResourceRef { path: _path }) => false,
        Value::Deferred(DeferredValue::BindingRef { binding: _binding }) => false,
        Value::Deferred(DeferredValue::FunctionCall {
            name: _name,
            args: _args,
            source_line: _source_line,
        }) => false,
        Value::Deferred(DeferredValue::Unavailable {
            name: _name,
            args: _args,
            reason: _reason,
            source_line: _source_line,
        }) => false,
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: _function,
            functions: _functions,
            args: _args,
            source_line: _source_line,
        }) => false,
        Value::Deferred(DeferredValue::Access(_access)) => false,
        Value::Deferred(DeferredValue::Unknown(_reason)) => false,
        Value::Deferred(DeferredValue::RedactedSecret { hash: _hash }) => false,
        Value::Deferred(DeferredValue::Placeholder(_placeholder)) => false,
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().all(module_call_value_is_context_free)
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().all(module_call_value_is_context_free)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().all(|part| {
            matches!(part, crate::resource::InterpolationPart::Literal(_literal))
                || matches!(part, crate::resource::InterpolationPart::Expr(value)
                    if module_call_value_is_context_free(value))
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => module_call_value_is_context_free(inner),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            module_call_value_is_context_free(value) && module_call_value_is_context_free(default)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            module_call_value_is_context_free(expression.lhs())
                && module_call_value_is_context_free(expression.rhs())
        }
    }
}

/// Collect module-call findings from authored source and its imported graph.
pub fn check_module_calls<E>(
    input: ModuleCallCheckInput<'_, '_, E, LoadedProviderContext>,
) -> Vec<ModuleCallFinding> {
    let provider_context = typecheck::TypecheckProviderContext::loaded(input.provider_context);
    check_module_calls_with_provider_context(input, provider_context)
}

/// Providerless module-call diagnostics for bootstrap and orphan-editor
/// contexts. Structural checks still run, while provider-owned leaves are not
/// sent to an unloaded resolver.
pub fn check_module_calls_providerless<E>(
    input: ModuleCallCheckInput<'_, '_, E, ProviderContext>,
) -> Vec<ModuleCallFinding> {
    let provider_context =
        typecheck::TypecheckProviderContext::providerless(input.provider_context);
    check_module_calls_with_provider_context(input, provider_context)
}

fn check_module_calls_with_provider_context<'a, 'bindings, E, C: ?Sized>(
    input: ModuleCallCheckInput<'a, 'bindings, E, C>,
    provider_context: typecheck::TypecheckProviderContext<'a>,
) -> Vec<ModuleCallFinding> {
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
        provider_context,
        schemas,
        findings: Vec::new(),
        active_modules: HashSet::new(),
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
    );
    checker.findings
}

/// Collect only findings whose authoritative inputs exist after expansion.
pub fn check_expanded_module_calls<P: PlaceholderKind>(
    input: ExpandedModuleCallCheckInput<'_, '_, P>,
) -> Vec<ModuleCallFinding> {
    let provider_context = ProviderContext::default();
    let schemas = SchemaRegistry::new();
    let mut checker = ModuleCallChecker {
        provider_context: typecheck::TypecheckProviderContext::providerless(&provider_context),
        schemas: &schemas,
        findings: Vec::new(),
        active_modules: HashSet::new(),
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
    config: typecheck::TypecheckProviderContext<'_>,
) -> Result<Vec<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let mut checked = Vec::with_capacity(export_params.len());
    let mut errors = Vec::new();

    for param in export_params {
        let mut resolved_param = param.clone();
        if let Some(value) = param.value() {
            let mut references_removed_secret = false;
            value.visit_resource_refs_with_usage(&mut |path, usage| {
                if usage == crate::resource::ResourceRefUsage::Required
                    && bindings.removed_attribute_was_sensitive(path.binding(), path.attribute())
                {
                    references_removed_secret = true;
                }
            });
            if references_removed_secret {
                errors.push(format!(
                    "export '{}' cannot hold a secret value",
                    param.name()
                ));
                checked.push(resolved_param);
                continue;
            }
            match crate::resolver::resolve_ref_value_with_config(
                value,
                bindings,
                config.as_provider_context(),
            ) {
                Ok(resolved) if crate::value::contains_secret(&resolved) => {
                    errors.push(format!(
                        "export '{}' cannot hold a secret value",
                        param.name()
                    ));
                }
                Ok(resolved) => {
                    resolved_param.set_value(Some(resolved));
                    if let (Some(type_expr), Some(value)) =
                        (resolved_param.type_expr_opt(), resolved_param.value())
                        && let Err(error) = config.check(
                            type_expr,
                            value,
                            typecheck::TypecheckInference::Unavailable,
                        )
                    {
                        errors.push(format!("export '{}': {error}", param.name()));
                    }
                }
                Err(error) => errors.push(format!("export '{}': {error}", param.name())),
            }
        }
        checked.push(resolved_param);
    }

    if errors.is_empty() {
        Ok(checked)
    } else {
        Err(errors.join("\n"))
    }
}

/// Resolve and structurally validate exports without loaded providers.
/// Provider-owned custom-type leaves are left unchecked, while container
/// shapes and provider-independent sibling values retain normal diagnostics.
pub fn check_export_values_pre_apply_providerless<E, P: PlaceholderKind, B: BindingAddress>(
    export_params: &[E],
    bindings: &crate::binding_index::ResolvedBindings<P, B>,
    config: &ProviderContext,
) -> Result<ExportsCheckedPreApply<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let display_params = resolve_and_validate_export_values(
        export_params,
        bindings,
        typecheck::TypecheckProviderContext::providerless(config),
    )?;
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
    config: &LoadedProviderContext,
) -> Result<ExportsCheckedPreApply<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let display_params = resolve_and_validate_export_values(
        export_params,
        bindings,
        typecheck::TypecheckProviderContext::loaded(config),
    )?;
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
    config: &LoadedProviderContext,
) -> Result<CheckedExportValues<E>, String>
where
    E: crate::parser::ExportParamLike<P, B> + Clone,
{
    let params = resolve_and_validate_export_values(
        &export_params.original_params,
        bindings,
        typecheck::TypecheckProviderContext::loaded(config),
    )?;
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
    let mut errors = Vec::new();

    for param in attribute_params {
        if let (Some(type_expr), Some(value)) = (&param.static_type, &param.value)
            && let Err(error) = typecheck::check_type_expr_value(
                type_expr,
                value,
                config,
                typecheck::TypecheckInference::Unavailable,
            )
        {
            errors.push(format!("attribute '{}': {}", param.name, error));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
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
        // `LiveComposition` has no directives — `directives()` is `None`
        // for that arm, so the depends_on walk is simply skipped.
        for dep in rref.directives().into_iter().flat_map(|d| &d.depends_on) {
            referenced.insert(dep.clone());
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
        if let Some(value) = &attr_param.value {
            collect_dependencies(value, &mut referenced);
        }
    }
    for export_param in &parsed.export_params {
        if let Some(value) = &export_param.value {
            collect_dependencies(value, &mut referenced);
        }
    }
    // Each `wait <target> { ... }` declaration references its target
    // and every binding in `depends_on = [...]`. The until predicate's
    // LHS is rooted at the target (enforced by parser), so the target
    // covers the LHS path too.
    for wb in &parsed.wait_bindings {
        referenced.insert(wb.target.clone());
        for dep in &wb.depends_on {
            referenced.insert(dep.clone());
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
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => return None,
        };
    }
    Some(current)
}

pub mod inference;

#[cfg(test)]
mod tests;
