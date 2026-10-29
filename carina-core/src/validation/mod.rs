//! Validation utilities for resources and modules

pub mod deferred_populate;
pub mod depends_on;
pub mod typecheck;
pub mod wait;

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::binding_index::{BindingIndex, RefType, RefTypeError, ResolvedRefType};
use crate::deps::collect_dependencies;
use crate::parser::{
    ForBinding, ModuleCall, ProviderContext, ResourceContext, ResourceRef, ResourceTypePath,
    SourceSpan, TypeExpr,
};
use crate::provider::ProviderFactory;
use crate::resource::{
    CompositionCallPath, ConcreteValue, DeferredValue, EnumValueResolver, RawEnumIdentifier,
    ReferencePath, ReferencePathRef, Resource, Value,
};
use crate::schema::{
    AttributeType, BindingKind, BindingShadowsEnumMemberError, FieldAncestry, SchemaRegistry,
    Shape, TypeError, TypeIdentity,
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
/// an id string until #3803 lands. Failed-inference sentinels remain unchecked.
pub fn lift_type_expr(type_expr: &TypeExpr) -> Option<AttributeType> {
    use crate::schema::StructField;

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
                        _ => None,
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
        TypeExpr::Unknown => None,
    }
}

static STRING_REF_SINK: TypeExpr = TypeExpr::String;

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
                    _ => None,
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
            Self::TypeExpr(_) | Self::String => None,
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
            Self::TypeExpr(_) | Self::String => None,
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
    CustomType(Box<crate::parser::CustomTypeValidationError>),
    Existing(String),
}

impl BindingResolutionError {
    pub fn type_error(&self) -> Option<&TypeError> {
        match self {
            Self::Type(error) => Some(error),
            Self::CustomType(_) | Self::Existing(_) => None,
        }
    }
}

impl std::fmt::Display for BindingResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Type(error) => error.fmt(formatter),
            Self::CustomType(error) => error.fmt(formatter),
            Self::Existing(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for BindingResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Type(error) => Some(error),
            Self::CustomType(error) => Some(error),
            Self::Existing(_) => None,
        }
    }
}

struct ExpectedEnumTarget<'a> {
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
}

/// Typed inputs shared by CLI and LSP while consuming parser-authored bare
/// binding uses. Provider context is deliberately mandatory: bootstrap parsing
/// may leave dotted declarations unresolved, but this validation seam may not.
pub struct ValidationTypeContext<'a> {
    pub schemas: &'a SchemaRegistry,
    pub provider_context: &'a ProviderContext,
    pub module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    scope_bindings: BindingIndex<'a>,
    inference_bindings: inference::InferenceBindings,
}

impl<'a> ValidationTypeContext<'a> {
    pub fn new<E>(
        file: &'a crate::parser::File<E>,
        schemas: &'a SchemaRegistry,
        provider_context: &'a ProviderContext,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self {
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
}

/// Shared classification consumed by CLI validation and LSP diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingUseSeverity {
    Error,
}

/// One typed failure tied directly to the parser token that authored the use.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingUseFinding {
    pub use_site: crate::parser::BareBindingUse,
    pub error: BindingResolutionError,
    severity: BindingUseSeverity,
}

impl BindingUseFinding {
    fn error(use_site: crate::parser::BareBindingUse, error: BindingResolutionError) -> Self {
        Self {
            use_site,
            error,
            severity: BindingUseSeverity::Error,
        }
    }

    pub fn severity(&self) -> BindingUseSeverity {
        self.severity
    }
}

impl std::fmt::Display for BindingUseFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: {} at line {}",
            self.use_site.source_file.path().display(),
            self.error,
            self.use_site.span.start_line(),
        )
    }
}

impl std::error::Error for BindingUseFinding {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Parser/validator consistency failure for a reserved value sink that has no
/// authored error finding. This should be unreachable for parser-produced
/// files, but it must block readiness rather than silently dropping a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingUseInvariantError {
    owner: crate::parser::BindingUseOwner,
    path: crate::parser::AttributePath,
}

impl BindingUseInvariantError {
    fn uncovered(slot: &crate::parser::PendingValueSlot) -> Self {
        Self {
            owner: slot.owner.clone(),
            path: slot.path.clone(),
        }
    }
}

impl std::fmt::Display for BindingUseInvariantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let field = self.path.root_name();
        match &self.owner {
            crate::parser::BindingUseOwner::Resource(address) => write!(
                formatter,
                "Carina could not validate attribute `{field}` on resource `{}` because its source expression was not recorded; please report this internal consistency error",
                address.resource_id(),
            ),
            crate::parser::BindingUseOwner::ModuleArgument { call, argument } => write!(
                formatter,
                "Carina could not validate module argument `{argument}` in `{}` at line {} because its source expression was not recorded; please report this internal consistency error",
                call.source_file().display(),
                call.call_span().start_line(),
            ),
        }
    }
}

impl std::error::Error for BindingUseInvariantError {}

/// Complete result of consuming one parsed file's out-of-band provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingUseReport {
    findings: Vec<BindingUseFinding>,
    invariant_errors: Vec<BindingUseInvariantError>,
}

impl std::fmt::Display for BindingUseReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, finding) in self.findings.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            finding.fmt(formatter)?;
        }
        for (index, invariant) in self.invariant_errors.iter().enumerate() {
            if !self.findings.is_empty() || index > 0 {
                formatter.write_str("\n")?;
            }
            invariant.fmt(formatter)?;
        }
        Ok(())
    }
}

impl std::error::Error for BindingUseReport {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Some(finding) = self.findings.first() {
            Some(finding as &(dyn std::error::Error + 'static))
        } else {
            self.invariant_errors
                .first()
                .map(|error| error as &(dyn std::error::Error + 'static))
        }
    }
}

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
    inner: crate::parser::File<E>,
}

impl<E> ResolvedFile<E> {
    pub fn as_file(&self) -> &crate::parser::File<E> {
        &self.inner
    }

    pub fn into_file(self) -> crate::parser::File<E> {
        self.inner
    }
}

impl<E> std::ops::Deref for ResolvedFile<E> {
    type Target = crate::parser::File<E>;

    fn deref(&self) -> &Self::Target {
        self.as_file()
    }
}

impl BindingUseReport {
    pub fn findings(&self) -> &[BindingUseFinding] {
        &self.findings
    }

    pub fn into_findings(self) -> Vec<BindingUseFinding> {
        self.findings
    }

    pub fn invariant_errors(&self) -> &[BindingUseInvariantError] {
        &self.invariant_errors
    }

    pub fn is_ready(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|finding| finding.severity() == BindingUseSeverity::Error)
            && self.invariant_errors.is_empty()
    }
}

fn binding_value_assignable_to_enum(
    value: &Value,
    declared_type: Option<&TypeExpr>,
    config: &ProviderContext,
    target: &ExpectedEnumTarget<'_>,
    resolver: &EnumValueResolver<'_>,
    identity: &TypeIdentity,
) -> Result<(), BindingResolutionError> {
    if let Some(declared_type) = declared_type {
        let resolved =
            resolve_type_expr(declared_type, config).map_err(BindingResolutionError::Existing)?;
        match resolved {
            TypeExpr::SchemaType { owned }
                if owned.identity().same_type(identity)
                    && owned.identity().assignable_to(identity) =>
            {
                return Ok(());
            }
            TypeExpr::Unknown => {}
            other => {
                return Err(BindingResolutionError::Type(TypeError::TypeMismatch {
                    expected: identity.to_string(),
                    got: other.to_string(),
                }));
            }
        }
    }
    match value {
        Value::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
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
        Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => resolver
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
        resolve_type_expr(declared_type, config).map_err(BindingResolutionError::Existing)?;
    if matches!(resolved, TypeExpr::Unknown) {
        return Ok(());
    }
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

fn reference_value_mismatch(
    reference: ReferencePathRef<'_>,
    target: crate::schema::TypeInSchema<'_>,
    bindings: &BindingIndex<'_>,
) -> Option<BindingResolutionError> {
    match bindings.ref_type(reference) {
        RefType::Typed(source) => source.is_assignable_to_sink(target).err().map(|mismatch| {
            BindingResolutionError::Existing(format!(
                "Type mismatch: expected {}, got {} ({})",
                target.resolved_type_name(),
                mismatch.actual_type_name(),
                mismatch.origin_description(reference),
            ))
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

fn deferred_value_mismatch(
    value: &Value,
    target: crate::schema::TypeInSchema<'_>,
    inference_bindings: &inference::InferenceBindings,
    schemas: &SchemaRegistry,
    bindings: &BindingIndex<'_>,
) -> Option<BindingResolutionError> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            reference_value_mismatch(ReferencePathRef::Access(path), target, bindings)
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            reference_value_mismatch(ReferencePathRef::Binding(binding), target, bindings)
        }
        Value::Deferred(
            DeferredValue::Coalesce { .. }
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => check_deferred_access_type(value, target, inference_bindings, schemas, bindings)
            .err()
            .map(|error| BindingResolutionError::Existing(error.to_string())),
        Value::Concrete(
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

fn precheck_value_source(
    value: &Value,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_>,
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
    )
}

fn resolve_value_source_against_sink(
    use_site: &crate::parser::BareBindingUse,
    value: &Value,
    declared_type: Option<&TypeExpr>,
    sink: RefSink<'_>,
    context: &ValidationTypeContext<'_>,
) -> Result<(), BindingResolutionError> {
    let prechecked = declared_type
        .is_none()
        .then(|| precheck_value_source(value, sink, context))
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
                context.provider_context,
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
            context.provider_context,
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
        RefSink::TypeExpr(type_expr) => typecheck::check_type_expr_value(
            type_expr,
            value,
            context.provider_context,
            typecheck::TypecheckInference::Unavailable,
        )
        .map_err(|error| BindingResolutionError::Existing(error.to_string())),
    }
}

fn resource_sink<'a, E>(
    file: &'a crate::parser::File<E>,
    address: &crate::parser::ResourceAddress,
    path: &crate::parser::AttributePath,
    schemas: &'a SchemaRegistry,
) -> Option<RefSink<'a>> {
    let resource = file
        .iter_all_resources()
        .find(|resource| resource.id() == address.resource_id())?;
    let schema = match resource {
        ResourceRef::LiveComposition(_) => return None,
        ResourceRef::DataSource(data_source) => schemas.get_for_data_source(data_source),
        ResourceRef::Resource(resource) | ResourceRef::Deferred { resource, .. } => {
            schemas.get_for(resource)
        }
    }?
    .schema();
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

fn module_argument_type<E>(
    file: &crate::parser::File<E>,
    call_id: &crate::parser::ModuleCallId,
    argument: &str,
    signatures: &crate::module_resolver::ResolvedModuleSignatures,
) -> Option<TypeExpr> {
    if let Some(call) = file.module_calls.iter().find(|call| call.id() == call_id) {
        return signatures
            .get(&call.module_name)?
            .arguments
            .iter()
            .find(|parameter| parameter.name == argument)
            .map(|parameter| parameter.type_expr.clone());
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
        .map(|argument| argument.declared_type().clone())
}

fn module_enum_declaration(
    expected: &TypeExpr,
    config: &ProviderContext,
) -> Result<Option<AttributeType>, BindingResolutionError> {
    let TypeExpr::SchemaType { owned } = expected else {
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
    match (expected, segment) {
        (TypeExpr::List(element), crate::parser::AttributePathSegment::ListIndex(_))
        | (TypeExpr::Map(element), crate::parser::AttributePathSegment::Key(_))
        | (TypeExpr::Map(element), crate::parser::AttributePathSegment::StructField(_)) => {
            Some((**element).clone())
        }
        (
            TypeExpr::Struct { fields },
            crate::parser::AttributePathSegment::Key(name)
            | crate::parser::AttributePathSegment::StructField(name),
        ) => fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, field_type)| field_type.clone()),
        (TypeExpr::Union(members), _) => exactly_one(
            members
                .iter()
                .filter_map(|member| descend_module_type_expr(member, segment)),
        ),
        (
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Unknown,
            _,
        )
        | (_, crate::parser::AttributePathSegment::Attribute(_)) => None,
        _ => None,
    }
}

fn resolve_module_argument_path_type(
    expected: &TypeExpr,
    path: &crate::parser::AttributePath,
    config: &ProviderContext,
) -> Result<Option<ResolvedParameterType>, BindingResolutionError> {
    let Some(resolved) = resolve_parameter_type_for_check(expected, config)
        .map_err(BindingResolutionError::Existing)?
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
        value => ResolvedParameterType::Value(value),
    }))
}

fn resolve_record_against_module(
    record: &crate::parser::ParsedBareBindingUse,
    expected: &TypeExpr,
    context: &ValidationTypeContext<'_>,
) -> Result<(), BindingResolutionError> {
    let Some(resolved) = resolve_module_argument_path_type(
        expected,
        &record.authored.path,
        context.provider_context,
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
                    context.provider_context,
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
pub fn binding_use_authority_owns_module_argument<E>(
    file: &crate::parser::File<E>,
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
pub fn module_argument_has_pending_binding_use<E>(
    file: &crate::parser::File<E>,
    call: &crate::parser::ModuleCallId,
    argument: &str,
) -> bool {
    file.bare_binding_uses().pending_slots().iter().any(|slot| {
        matches!(
            &slot.owner,
            crate::parser::BindingUseOwner::ModuleArgument {
                call: owner,
                argument: owner_argument,
            } if owner == call && owner_argument == argument
        )
    })
}

/// Report-only schema-aware authority for every parser-recorded bare binding
/// use. Editor and collect-all validation paths borrow the file through this
/// function; they do not mint production readiness.
pub fn check_bare_binding_uses<E>(
    file: &crate::parser::File<E>,
    context: &ValidationTypeContext<'_>,
) -> BindingUseReport {
    let mut report = BindingUseReport {
        findings: Vec::new(),
        invariant_errors: Vec::new(),
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
            crate::parser::BindingUseOwner::ModuleArgument { call, argument } => {
                match module_argument_type(file, call, argument, context.module_signatures) {
                    Some(expected) => resolve_record_against_module(record, &expected, context),
                    None => match &record.source {
                        crate::parser::BareBindingSource::Value { .. } => Ok(()),
                        crate::parser::BareBindingSource::Structural(handle) => Err(
                            BindingResolutionError::Existing(handle.bare_error().to_string()),
                        ),
                    },
                }
            }
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

    for slot in file.bare_binding_uses().pending_slots() {
        let has_erroring_finding = report.findings.iter().any(|finding| {
            finding.severity() == BindingUseSeverity::Error
                && finding.use_site.owner == slot.owner
                && finding.use_site.path.root_name() == slot.path.root_name()
        });
        if !has_erroring_finding {
            report
                .invariant_errors
                .push(BindingUseInvariantError::uncovered(slot));
        }
    }

    report
}

/// Consume the exact file checked by the bare-binding authority.
///
/// Success wraps the exact checked file without exposing a mutable provenance
/// channel, so no later caller can pair an unrelated file with a detached
/// proof. On failure the report contains every authored finding.
pub fn resolve_bare_binding_uses<E>(
    file: crate::parser::File<E>,
    schemas: &SchemaRegistry,
    provider_context: &ProviderContext,
    module_signatures: &crate::module_resolver::ResolvedModuleSignatures,
) -> Result<ResolvedFile<E>, BindingUseReport> {
    let report = {
        let context =
            ValidationTypeContext::new(&file, schemas, provider_context, module_signatures);
        check_bare_binding_uses(&file, &context)
    };
    if !report.is_ready() {
        return Err(report);
    }

    Ok(ResolvedFile { inner: file })
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
pub(crate) fn visit_refs_with_sink<'a>(
    value: &Value,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(
        ReferencePathRef<'_>,
        Option<RefSink<'a>>,
        crate::resource::ResourceRefUsage,
        Option<&Value>,
    ),
) {
    visit_refs_with_sink_inner(value, sink, crate::resource::ResourceRefUsage::Required, f);
}

fn visit_refs_with_sink_inner<'a>(
    value: &Value,
    sink: Option<RefSink<'a>>,
    usage: crate::resource::ResourceRefUsage,
    f: &mut impl FnMut(
        ReferencePathRef<'_>,
        Option<RefSink<'a>>,
        crate::resource::ResourceRefUsage,
        Option<&Value>,
    ),
) {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            f(ReferencePathRef::Access(path), sink, usage, None)
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            f(ReferencePathRef::Binding(binding), sink, usage, None)
        }
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::UnknownReason::ModuleArgument { name, .. },
        )) => f(ReferencePathRef::Binding(name), sink, usage, None),
        Value::Concrete(ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                visit_refs_with_sink_inner(item, child_sink, usage, f);
            }
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                let child_sink = sink.and_then(|sink| sink.map_entry(key));
                visit_refs_with_sink_inner(value, child_sink, usage, f);
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    visit_refs_with_sink_inner(value, Some(RefSink::String), usage, f);
                }
            }
        }
        Value::Deferred(
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
        Value::Deferred(DeferredValue::Access(access)) => {
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
        Value::Deferred(DeferredValue::Secret(inner)) => {
            visit_refs_with_sink_inner(inner, sink, usage, f)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
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
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_))
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => {}
    }
}

/// Visit values whose postfix path needs inference in addition to the normal
/// reference walk. Computed Access values need their result checked; a
/// canonical ResourceRef with path segments also needs inference so a known
/// scalar cannot be indexed merely because BindingIndex reserves shape
/// diagnostics for upstream references.
fn visit_computed_accesses_with_sink<'a>(
    value: &Value,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(&Value, Option<RefSink<'a>>),
) {
    match value {
        Value::Deferred(DeferredValue::Access(access)) => {
            if crate::resource::module_argument_reference_path(value).is_none() {
                f(value, sink);
                visit_computed_accesses_with_sink(access.base(), None, f);
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) if !path.segments().is_empty() => {
            f(value, sink);
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                visit_computed_accesses_with_sink(item, child_sink, f);
            }
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                visit_computed_accesses_with_sink(
                    value,
                    sink.and_then(|sink| sink.map_entry(key)),
                    f,
                );
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    visit_computed_accesses_with_sink(value, Some(RefSink::String), f);
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for argument in args {
                visit_computed_accesses_with_sink(argument.walker_value(), None, f);
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            visit_computed_accesses_with_sink(inner, sink, f)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            visit_computed_accesses_with_sink(value, sink, f);
            visit_computed_accesses_with_sink(default, sink, f);
        }
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_))
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => {}
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

/// Check a deferred postfix result against its sink with the canonical
/// inference authority. Unknown bases remain unchecked; known-invalid access
/// and known result mismatches are errors.
pub fn check_deferred_access_type(
    value: &Value,
    expected: crate::schema::TypeInSchema<'_>,
    bindings: &inference::InferenceBindings,
    schemas: &SchemaRegistry,
    binding_index: &BindingIndex<'_>,
) -> Result<(), DeferredAccessTypeError> {
    // Canonical reference-rooted postfix expressions are ResourceRefs. Route
    // those through BindingIndex so managed resources and wait aliases share
    // the same schema/value-source authority. This decision belongs here,
    // rather than in every CLI/LSP caller, so a new binding kind cannot be
    // silently skipped by a copied `InferenceBinding` predicate.
    if let Value::Deferred(DeferredValue::ResourceRef { path }) = value {
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
        Err(error @ inference::InferenceError::InvalidAccess { .. })
        | Err(error @ inference::InferenceError::UnknownAttribute { .. }) => {
            return Err(DeferredAccessTypeError::InvalidAccess { error });
        }
        Err(inference::InferenceError::IncompatibleCoalesceTypes { left, default }) => {
            return Err(DeferredAccessTypeError::TypeMismatch {
                expected: left,
                actual: default,
                origin: "from coalesce default".to_string(),
            });
        }
        // Dynamic result types and unavailable authorities cannot be checked
        // soundly at this phase. Spell every inference variant out so adding a
        // new failure reason forces an explicit checked/unchecked decision.
        Err(inference::InferenceError::UnknownType { .. })
        | Err(inference::InferenceError::HeterogeneousCollection { .. })
        | Err(inference::InferenceError::UnknownBinding { .. })
        | Err(inference::InferenceError::NonInferableBinding { .. })
        | Err(inference::InferenceError::SchemaUnavailable { .. })
        | Err(inference::InferenceError::CyclicVirtualBinding { .. }) => return Ok(()),
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

fn resolve_resource_reference(
    resource_id: &crate::resource::ResourceId,
    reference: ReferencePathRef<'_>,
    bindings: &BindingIndex<'_>,
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
pub struct ModuleCallRefError {
    call_label: String,
    pub argument: String,
    /// Parser-authored key span for `argument`.
    pub argument_span: SourceSpan,
    pub path: ReferencePath,
    pub kind: ModuleCallRefErrorKind,
}

fn format_module_call_ref_error(
    f: &mut std::fmt::Formatter<'_>,
    call_label: &str,
    argument: &str,
    path: &ReferencePath,
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
                Some(declared) => format!("from {}, declared {}", path.to_dot_string(), declared),
                None => format!("from {}", path.to_dot_string()),
            },
        ),
        ModuleCallRefErrorKind::UnknownAttribute(error) => {
            write!(f, "module call '{}': {}", call_label, error)
        }
    }
}

impl std::fmt::Display for ModuleCallRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format_module_call_ref_error(f, &self.call_label, &self.argument, &self.path, &self.kind)
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
                .map(|argument| &argument.type_expr);

            visit_refs_with_sink(
                value.walker_value(),
                declared_sink.map(RefSink::TypeExpr),
                &mut |path, sink, usage, _candidate| {
                    let Some(kind) =
                        composition_ref_error_kind(path, sink, bindings, unknown_policy, usage)
                    else {
                        return;
                    };
                    errors.push(ModuleCallRefError {
                        call_label: call_name.to_string(),
                        argument: argument_name.to_string(),
                        argument_span,
                        path: path.to_owned(),
                        kind,
                    });
                },
            );
        }
    }

    errors
}

#[derive(Debug, Clone)]
pub struct AttributeParamRefError {
    pub attribute: String,
    pub path: ReferencePath,
    pub kind: ModuleCallRefErrorKind,
}

impl std::fmt::Display for AttributeParamRefError {
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

impl std::error::Error for AttributeParamRefError {}

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
    pub path: ReferencePath,
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
#[derive(Debug, Clone)]
pub struct CompositionAttributeRefError {
    pub error: AttributeParamRefError,
    pub module_name: String,
    pub module_source: Option<String>,
    pub module_directory: Option<std::path::PathBuf>,
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

fn composition_ref_error_kind(
    path: ReferencePathRef<'_>,
    sink: Option<RefSink<'_>>,
    bindings: &BindingIndex<'_>,
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
pub fn validate_user_function_ref_types_with_bindings<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    validate_user_function_ref_types_inner(parsed, bindings, UnknownReferencePolicy::Report)
}

fn validate_user_function_ref_types_inner<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
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
            _ => None,
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
            _ => None,
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
            args: &[crate::resource::FunctionArgument],
            function: Option<&crate::parser::UserFunction>,
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
            | Value::Deferred(DeferredValue::Unknown(_)) => {}
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
            check_function_calls(
                value,
                Some(&parameter.type_expr),
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
            check_function_calls(
                value,
                parameter.type_expr.as_ref(),
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
            check_function_calls(
                value,
                parameter.type_expr_opt(),
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
    for wait in &parsed.wait_bindings {
        check_function_calls(
            &wait.until_predicate.rhs,
            None,
            None,
            bindings,
            unknown_policy,
            &mut errors,
        );
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
pub fn validate_composition_ref_types_with_bindings(
    compositions: &[crate::resource::LiveComposition],
    bindings: &BindingIndex<'_>,
) -> Vec<CompositionRefError> {
    let mut errors = Vec::new();

    for composition in compositions {
        let call_path = composition.call_path();
        let call = call_path.immediate();
        for (argument_name, argument) in &composition.signature.arguments {
            let (declared_type, origin) = argument.validation_metadata();
            let mut reference_position = 0;
            visit_refs_with_sink(
                argument.value(),
                Some(RefSink::TypeExpr(declared_type)),
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
            visit_refs_with_sink(
                &value,
                attribute.declared_type().map(RefSink::TypeExpr),
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
                            reference_position: current_position,
                        },
                    ));
                },
            );
        }
    }

    let mut seen_calls = HashSet::new();
    let mut seen_attributes = HashSet::new();
    errors.retain(|error| match error {
        CompositionRefError::ModuleCall(error) => {
            seen_calls.insert(CompositionModuleCallErrorKey {
                call_path: error.call_path.clone(),
                argument: error.argument.clone(),
                reference_position: error.reference_position,
                kind: (&error.kind).into(),
            })
        }
        CompositionRefError::Attribute(error) => {
            seen_attributes.insert(CompositionAttributeErrorKey {
                module_identity: error.module_identity(),
                attribute: error.error.attribute.clone(),
                reference_position: error.reference_position,
                kind: (&error.error.kind).into(),
            })
        }
    });
    errors
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
pub fn validate_anonymous_resource_schemas<'a>(
    resources: impl IntoIterator<Item = &'a Resource>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
) -> Vec<AnonymousResourceSchemaError> {
    let mut grouped = IndexMap::<AnonymousResourceSchemaKey, usize>::new();
    for resource in resources.into_iter().filter(|resource| {
        resource.binding.is_none()
            && resource.id.identity_str().is_none()
            && registry.get_for(resource).is_none()
            && !known_providers.contains(&resource.id.provider)
    }) {
        let key = AnonymousResourceSchemaKey {
            provider: resource.id.provider.clone(),
            resource_type: resource.id.resource_type.clone(),
            provider_instance: resource.id.provider_instance.clone(),
        };
        *grouped.entry(key).or_default() += 1;
    }

    grouped
        .into_iter()
        .map(|(key, count)| AnonymousResourceSchemaError { key, count })
        .collect()
}

/// Validate resources against their schemas.
///
/// Two-sided check: a `read` resource requires a `DataSource` registry entry,
/// and a non-`read` resource requires a `Managed` registry entry. If the
/// wrong-kind entry is present (e.g. `read` against a managed-only type),
/// emit a kind-specific error explaining the mismatch.
pub fn validate_resources<E>(
    parsed: &crate::parser::File<E>,
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
            validate_anonymous_resource_schemas(
                parsed
                    .iter_all_resources()
                    .filter_map(|resource| match resource {
                        ResourceRef::Resource(resource)
                        | ResourceRef::Deferred { resource, .. } => Some(resource),
                        ResourceRef::LiveComposition(_) | ResourceRef::DataSource(_) => None,
                    }),
                registry,
                known_providers,
            )
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
    enum ValidatableKind {
        Resource,
        DataSource,
    }
    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed —
        // `for` bodies never carry `read` / composition.
        let (kind, schema) = match rref {
            ResourceRef::LiveComposition(_) => continue,
            ResourceRef::DataSource(d) => {
                (ValidatableKind::DataSource, registry.get_for_data_source(d))
            }
            ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                (ValidatableKind::Resource, registry.get_for(m))
            }
        };
        let id = rref.id();
        let quoted_string_attrs = rref.quoted_string_attrs();

        match schema {
            Some(schema) => {
                let is_string_literal = |attr: &str| quoted_string_attrs.contains(attr);
                if let Err(errors) = schema.validate_with_resolver(
                    &rref.resolved_attributes(),
                    &is_string_literal,
                    provider_context.custom_type_resolver(),
                ) {
                    for error in errors {
                        all_errors.push(format!("{}: {}", id, error));
                    }
                }
            }
            None => {
                let provider = id.provider.as_str();
                let resource_type = id.resource_type.as_str();

                // No matching-kind entry. Skip if provider is not loaded —
                // schemas are simply not available, not a configuration error.
                if !provider.is_empty() && !known_providers.contains(provider) {
                    continue;
                }
                let has_managed = registry.has_managed(provider, resource_type);
                let has_data_source = registry.has_data_source(provider, resource_type);
                let kind_label = if provider.is_empty() {
                    resource_type.to_string()
                } else {
                    format!("{}.{}", provider, resource_type)
                };

                match kind {
                    ValidatableKind::DataSource if has_managed => {
                        // `read` used against a managed-only type
                        all_errors.push(format!(
                            "{} is a managed resource, not a data source. Remove the `read` keyword:\n  let <name> = {} {{ }}",
                            kind_label, kind_label
                        ));
                    }
                    ValidatableKind::Resource if has_data_source => {
                        // No `read` against a data-source-only type
                        all_errors.push(format!(
                            "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                            kind_label, kind_label
                        ));
                    }
                    _ => {
                        all_errors.push(format!("Unknown resource type: {}", kind_label));
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
pub fn validate_resource_ref_types<E>(
    parsed: &crate::parser::File<E>,
    registry: &SchemaRegistry,
    _argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    validate_resource_ref_types_inner(parsed, registry, bindings)
}

fn validate_resource_ref_types_inner<E>(
    parsed: &crate::parser::File<E>,
    registry: &SchemaRegistry,
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();
    let inference_bindings = inference::bindings_from_parsed(parsed);

    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed.
        let schema = match rref {
            ResourceRef::LiveComposition(_) => continue,
            ResourceRef::DataSource(d) => registry.get_for_data_source(d),
            ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                registry.get_for(m)
            }
        };
        let Some(validated_schema) = schema else {
            continue;
        };
        let resource_id = rref.id();
        let resource_location = match rref.context() {
            ResourceContext::Direct => resource_id.to_string(),
            ResourceContext::Deferred(deferred) => {
                format!("for-body `{}` {}", deferred.header, resource_id)
            }
        };

        let attrs = rref.attributes();
        for (attr_name, attr_value) in attrs.iter() {
            let sink = validated_schema
                .effective_attribute_by_authored_name(attr_name)
                .map(|attribute| RefSink::from_effective_type(attribute.value_type()));
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
                let sink_type = match sink {
                    Some(RefSink::EffectiveType {
                        attr_type, defs, ..
                    }) => attr_type.in_schema(defs),
                    Some(RefSink::String) => crate::schema::TypeInSchema::schemaless(&string_sink),
                    Some(RefSink::TypeExpr(_)) | None => return,
                };
                match check_deferred_access_type(
                    access,
                    sink_type,
                    &inference_bindings,
                    registry,
                    bindings,
                ) {
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
        }
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

fn validate_deferred_for_iterable_types_with_bindings<E>(
    parsed: &crate::parser::File<E>,
    schemas: &SchemaRegistry,
    bindings: &inference::InferenceBindings,
) -> Vec<DeferredForIterableTypeDiagnostic> {
    let mut diagnostics = Vec::new();
    for deferred in &parsed.deferred_for_expressions {
        let expected = match &deferred.binding {
            ForBinding::Simple(_) | ForBinding::Indexed(_, _) => DeferredIterableKind::List,
            ForBinding::Map(_, _) => DeferredIterableKind::Map,
        };
        validate_deferred_iterable_operand(
            &deferred.iterable,
            expected,
            bindings,
            schemas,
            deferred,
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

fn validate_deferred_iterable_operand(
    value: &Value,
    expected: DeferredIterableKind,
    bindings: &inference::InferenceBindings,
    schemas: &SchemaRegistry,
    deferred: &crate::parser::DeferredForExpression,
    diagnostics: &mut Vec<DeferredForIterableTypeDiagnostic>,
) {
    match value {
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            validate_deferred_iterable_operand(
                value,
                expected,
                bindings,
                schemas,
                deferred,
                diagnostics,
            );
            validate_deferred_iterable_operand(
                default,
                expected,
                bindings,
                schemas,
                deferred,
                diagnostics,
            );
            return;
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            validate_deferred_iterable_operand(
                inner,
                expected,
                bindings,
                schemas,
                deferred,
                diagnostics,
            );
            return;
        }
        _ => {}
    }

    let inferred = match value {
        Value::Concrete(ConcreteValue::List(_)) => {
            Some(TypeExpr::List(Box::new(TypeExpr::Unknown)))
        }
        Value::Concrete(ConcreteValue::Map(_)) => Some(TypeExpr::Map(Box::new(TypeExpr::Unknown))),
        other => match inference::infer_type_from_value(other, bindings, schemas) {
            Ok(inferred) => Some(inferred),
            Err(inference::InferenceError::IncompatibleCoalesceTypes { left, default }) => {
                diagnostics.push(deferred_iterable_diagnostic(
                    deferred,
                    format!("coalesce default type `{default}` does not match left type `{left}`"),
                ));
                None
            }
            Err(inference::InferenceError::HeterogeneousCollection { reason }) => {
                diagnostics.push(deferred_iterable_diagnostic(deferred, reason));
                None
            }
            Err(
                inference::InferenceError::UnknownType { .. }
                | inference::InferenceError::UnknownBinding { .. }
                | inference::InferenceError::NonInferableBinding { .. }
                | inference::InferenceError::UnknownAttribute { .. }
                | inference::InferenceError::SchemaUnavailable { .. }
                | inference::InferenceError::InvalidAccess { .. }
                | inference::InferenceError::CyclicVirtualBinding { .. },
            ) => None,
        },
    };

    let Some(inferred) = inferred else {
        return;
    };
    let matches = matches!(
        (expected, &inferred),
        (DeferredIterableKind::List, TypeExpr::List(_))
            | (DeferredIterableKind::Map, TypeExpr::Map(_))
    );
    if !matches {
        diagnostics.push(deferred_iterable_diagnostic(
            deferred,
            format!("expected {expected} iterable, got {inferred}"),
        ));
    }
}

fn deferred_iterable_diagnostic(
    deferred: &crate::parser::DeferredForExpression,
    message: String,
) -> DeferredForIterableTypeDiagnostic {
    DeferredForIterableTypeDiagnostic {
        file: deferred.file.clone(),
        line: deferred.line,
        expression: crate::value::format_value(&deferred.iterable),
        message,
    }
}

/// Validate that attribute parameter ResourceRef values name attributes on
/// their referenced schemas, and that plain refs with a simple custom type
/// annotation are type-compatible.
///
/// For example, `attributes { role_arn: iam_role_arn = role.role_name }` should
/// be rejected because `role_name` is `String`, not `IamRoleArn`.
pub fn validate_attribute_param_ref_types_with_bindings(
    attribute_params: &[crate::parser::AttributeParameter],
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    validate_attribute_param_ref_types_inner(
        attribute_params,
        bindings,
        UnknownReferencePolicy::Report,
    )
}

fn validate_attribute_param_ref_types_inner(
    attribute_params: &[crate::parser::AttributeParameter],
    bindings: &BindingIndex<'_>,
    unknown_policy: UnknownReferencePolicy,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in attribute_params {
        let Some(ref value) = param.value else {
            continue;
        };

        visit_refs_with_sink(
            value,
            param.type_expr.as_ref().map(RefSink::TypeExpr),
            &mut |path, sink, usage, _candidate| {
                check_attribute_param_ref(
                    &param.name,
                    sink.and_then(RefSink::as_type_expr),
                    path,
                    bindings,
                    unknown_policy,
                    usage,
                    &mut errors,
                );
            },
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_attribute_param_ref(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: ReferencePathRef<'_>,
    bindings: &BindingIndex<'_>,
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
pub fn validate_export_param_ref_types_with_bindings(
    export_params: &[crate::parser::InferredExportParam],
    bindings: &BindingIndex<'_>,
    reported_inference_error_indices: &HashSet<usize>,
) -> Result<(), String> {
    validate_export_param_ref_types_inner(
        export_params,
        bindings,
        Some(reported_inference_error_indices),
        UnknownReferencePolicy::Report,
    )
}

fn validate_export_param_ref_types_inner<E: crate::parser::ExportParamLike>(
    export_params: &[E],
    bindings: &BindingIndex<'_>,
    reported_inference_error_indices: Option<&HashSet<usize>>,
    unknown_policy: UnknownReferencePolicy,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for (index, param) in export_params.iter().enumerate() {
        let Some(value) = param.value() else {
            continue;
        };
        let type_expr = param.type_expr_opt();
        let unknown_type = matches!(type_expr, Some(crate::parser::TypeExpr::Unknown));
        // Inference and this walk share one dedup rule: if inference has
        // already reported this exact export-param index, skip its Unknown
        // existence walk entirely. Names are display text, not identity, and
        // can collide across sibling files.
        if unknown_type
            && reported_inference_error_indices.is_some_and(|reported| reported.contains(&index))
        {
            continue;
        }

        let sink = type_expr
            .filter(|type_expr| !matches!(type_expr, crate::parser::TypeExpr::Unknown))
            .map(RefSink::TypeExpr);
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

fn check_export_ref(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: ReferencePathRef<'_>,
    bindings: &BindingIndex<'_>,
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
pub(crate) fn validate_expansion_boundary_ref_types_with_bindings<
    E: crate::parser::ExportParamLike,
>(
    parsed: &crate::parser::File<E>,
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    let mut errors = Vec::new();

    if let Err(message) = validate_attribute_param_ref_types_inner(
        &parsed.attribute_params,
        bindings,
        UnknownReferencePolicy::Defer,
    ) {
        errors.extend(message.lines().map(str::to_string));
    }
    if let Err(message) = validate_export_param_ref_types_inner(
        &parsed.export_params,
        bindings,
        None,
        UnknownReferencePolicy::Defer,
    ) {
        errors.extend(message.lines().map(str::to_string));
    }
    errors.extend(
        validate_module_call_argument_ref_types_inner(
            &parsed.module_calls,
            imported_modules,
            bindings,
            UnknownReferencePolicy::Defer,
        )
        .into_iter()
        .map(|error| error.to_string()),
    );
    if let Err(message) =
        validate_user_function_ref_types_inner(parsed, bindings, UnknownReferencePolicy::Defer)
    {
        errors.extend(message.lines().map(str::to_string));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
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
    members: &crate::schema::UnionMembers<'a>,
    visited_refs: &mut Vec<&'a str>,
    mut predicate: impl FnMut(crate::schema::ResolvedUnionMember<'a>, &[&'a str]) -> bool,
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
        Shape::String { .. } | Shape::Enum { .. } => true,
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

/// Reject module-level type declarations whose type position names an
/// unknown bare custom type (carina#3239).
///
/// Walks every typed parameter `parsed` carries — `arguments`,
/// `attributes`, `exports` (when typed) — and applies the same
/// predicate the parser's loaded custom-type state uses.
///
/// The parser already rejects unknown `TypeExpr::Simple` names when
/// it is handed a `ProviderContext` with `ProviderCustomTypes::Loaded`. That
/// gate fires for every parse path *after* the provider-registration
/// phase has populated the context — imported modules re-parsed by
/// `module_resolver::resolve_modules_with_config` and every LSP
/// diagnostic pass.
///
/// The root-config parse is the one exception: `load_configuration_with_config`
/// runs with the bootstrap `ProviderCustomTypes::Unloaded` context because
/// schemas have not been collected yet, so a standalone-module
/// validate (`carina validate ./my_module/`) would let an unknown
/// custom-type name in `arguments { foo: TotallyMadeUpType }` slip
/// through. This post-parse walk re-applies the same predicate against
/// the now-enriched context, closing the gap without re-parsing.
///
/// `attributes` and `exports` are covered for the same reason as
/// `arguments`: all three are module-boundary type declarations, all
/// three are reached through the same root-config parse, and an
/// unknown bare custom type in any of them surfaces the identical
/// silent-accept bug.
///
/// The check is restricted to bare PascalCase names that parsed as
/// `TypeExpr::Simple`. Dotted type expressions (`aws.iam.Role.Arn`)
/// are resolved separately by the dotted-type validation pass.
pub fn validate_argument_custom_types<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    for arg in &parsed.arguments {
        collect_unknown_simple_types_in(&arg.type_expr, config, "argument", &arg.name, &mut errors);
    }
    for ap in &parsed.attribute_params {
        if let Some(ty) = &ap.type_expr {
            collect_unknown_simple_types_in(ty, config, "attribute", &ap.name, &mut errors);
        }
    }
    for ep in &parsed.export_params {
        if let Some(ty) = ep.type_expr_opt() {
            collect_unknown_simple_types_in(ty, config, "export", ep.name(), &mut errors);
        }
    }
    errors
}

fn resolve_dotted_unresolved(
    path: &ResourceTypePath,
    config: &ProviderContext,
) -> Result<TypeExpr, String> {
    if config.has_resource_type(&path.provider, &path.resource_type) {
        return Ok(TypeExpr::Ref(path.clone()));
    }

    let mut authored_parts = path
        .resource_type
        .split('.')
        .map(String::from)
        .collect::<Vec<_>>();
    let Some(kind) = authored_parts.pop() else {
        return Err(crate::parser::unknown_custom_type_message(
            &path.to_string(),
            config,
        ));
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

    Err(crate::parser::unknown_custom_type_message(
        &path.to_string(),
        config,
    ))
}

/// Resolve parser-produced dotted type paths against an enriched provider
/// context, recursing through every nested type-expression container.
pub fn resolve_type_expr(ty: &TypeExpr, config: &ProviderContext) -> Result<TypeExpr, String> {
    match ty {
        TypeExpr::DottedUnresolved(path) => resolve_dotted_unresolved(path, config),
        TypeExpr::Simple(snake) => crate::parser::resolve_known_bare_custom_type(snake, config)
            .map(|resolved| resolved.unwrap_or_else(|| ty.clone())),
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
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Unknown => Ok(ty.clone()),
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
pub(crate) fn check_handle_sink(
    handle: &crate::eval_value::StructuralHandle,
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

/// Public bridge used by editor diagnostics without exposing the parser-only
/// handle representation. `None` means the argument is a value; `Some` means
/// the shared handle checker made the complete decision.
pub fn check_module_argument_handle_sink(
    argument: &crate::parser::ModuleArgument,
    resolved_parameter: &ResolvedParameterType,
) -> Option<Result<(), HandleSinkError>> {
    argument
        .handle()
        .map(|handle| check_handle_sink(handle, resolved_parameter))
}

/// Resolve and classify a module or function parameter type.
///
/// Callers must not fall back to inspecting the raw [`TypeExpr`] on error:
/// in particular, [`TypeExpr::DottedUnresolved`] can name either a resource
/// handle or a provider-defined value type. Bootstrap parser paths may retain
/// an explicitly provisional binding until an enriched parse is available.
pub fn resolve_parameter_type(
    ty: &TypeExpr,
    config: &ProviderContext,
) -> Result<ResolvedParameterType, String> {
    match resolve_type_expr(ty, config)? {
        TypeExpr::Ref(path) => Ok(ResolvedParameterType::Handle(path)),
        resolved => Ok(ResolvedParameterType::Value(resolved)),
    }
}

/// Resolve a parameter type when the provider catalog is authoritative.
/// Bootstrap parsing deliberately defers an unresolved dotted annotation;
/// the enriched validation pass repeats the check after schemas are loaded.
pub fn resolve_parameter_type_for_check(
    ty: &TypeExpr,
    config: &ProviderContext,
) -> Result<Option<ResolvedParameterType>, String> {
    match resolve_parameter_type(ty, config) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(_) if !config.custom_types_loaded() => Ok(None),
        Err(message) => Err(message),
    }
}

/// Resolve every module-boundary type declaration in place. Diagnostics
/// include the declaration kind/name so callers can report the same surface
/// as [`validate_argument_custom_types`].
pub fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    for arg in &mut parsed.arguments {
        match resolve_type_expr(&arg.type_expr, config) {
            Ok(resolved) => arg.type_expr = resolved,
            Err(e) => errors.push(format!("argument '{}': {e}", arg.name)),
        }
    }
    for ap in &mut parsed.attribute_params {
        if let Some(ty) = &ap.type_expr {
            match resolve_type_expr(ty, config) {
                Ok(resolved) => ap.type_expr = Some(resolved),
                Err(e) => errors.push(format!("attribute '{}': {e}", ap.name)),
            }
        }
    }
    for ep in &mut parsed.export_params {
        let export_name = ep.name().to_string();
        if let Some(ty) = ep.type_expr_opt_mut() {
            match resolve_type_expr(ty, config) {
                Ok(resolved) => *ty = resolved,
                Err(e) => errors.push(format!("export '{export_name}': {e}")),
            }
        }
    }
    for function in parsed.user_functions.values_mut() {
        for parameter in &mut function.params {
            if let Some(ty) = &parameter.param_type {
                match resolve_type_expr(ty, config) {
                    Ok(resolved) => parameter.param_type = Some(resolved),
                    Err(error) => errors.push(format!(
                        "function '{}' parameter '{}': {error}",
                        function.name, parameter.name
                    )),
                }
            }
        }
        if let Some(ty) = &function.return_type {
            match resolve_type_expr(ty, config) {
                Ok(resolved) => function.return_type = Some(resolved),
                Err(error) => {
                    errors.push(format!("function '{}' return type: {error}", function.name))
                }
            }
        }
    }
    errors
}

/// Resolve declarations with an intentionally incomplete provider context.
///
/// Upstream export discovery runs in the consumer process, whose providers
/// need not include every provider used by the upstream project. A type that
/// cannot be named in that context carries no sound consumer-side constraint,
/// so replace it with [`TypeExpr::Unknown`] rather than interpreting the raw
/// dotted spelling as a resource handle.
pub(crate) fn resolve_file_type_exprs_or_unknown<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
) {
    for arg in &mut parsed.arguments {
        arg.type_expr = resolve_type_expr(&arg.type_expr, config).unwrap_or(TypeExpr::Unknown);
    }
    for parameter in &mut parsed.attribute_params {
        if let Some(type_expr) = &mut parameter.type_expr {
            *type_expr = resolve_type_expr(type_expr, config).unwrap_or(TypeExpr::Unknown);
        }
    }
    for parameter in &mut parsed.export_params {
        if let Some(type_expr) = parameter.type_expr_opt_mut() {
            *type_expr = resolve_type_expr(type_expr, config).unwrap_or(TypeExpr::Unknown);
        }
    }
}

/// Recursively walk a [`TypeExpr`] and push one diagnostic per
/// [`TypeExpr::Simple`] whose name is not a known bare custom type
/// under `config`. Helper for [`validate_argument_custom_types`].
///
/// Each emitted message is a single line (no embedded newlines) so the
/// caller can `split('\n')` to lift findings into individual errors.
///
/// Variants that carry no nested `TypeExpr` are listed explicitly
/// rather than caught by a wildcard: a future variant that *does* nest
/// a `TypeExpr` should be a compile error here, not a silent
/// type-checking gap.
fn collect_unknown_simple_types_in(
    ty: &crate::parser::TypeExpr,
    config: &ProviderContext,
    decl_kind: &str,
    decl_name: &str,
    errors: &mut Vec<String>,
) {
    use crate::parser::TypeExpr;
    match ty {
        TypeExpr::Simple(snake) => {
            match crate::parser::resolve_known_bare_custom_type(snake, config) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    let pascal = crate::parser::snake_to_pascal(snake);
                    errors.push(format!(
                        "{decl_kind} '{decl_name}': {}",
                        crate::parser::unknown_custom_type_message(&pascal, config)
                    ));
                }
                Err(message) => errors.push(format!("{decl_kind} '{decl_name}': {message}")),
            }
        }
        TypeExpr::DottedUnresolved(path) => {
            if let Err(e) = resolve_dotted_unresolved(path, config) {
                errors.push(format!("{decl_kind} '{decl_name}': {e}"));
            }
        }
        TypeExpr::List(inner) | TypeExpr::Map(inner) => {
            collect_unknown_simple_types_in(inner, config, decl_kind, decl_name, errors);
        }
        TypeExpr::Union(members) => {
            for m in members {
                collect_unknown_simple_types_in(m, config, decl_kind, decl_name, errors);
            }
        }
        TypeExpr::Struct { fields } => {
            for (_, field_ty) in fields {
                collect_unknown_simple_types_in(field_ty, config, decl_kind, decl_name, errors);
            }
        }
        // Leaves with no nested `TypeExpr` to recurse into. Listed
        // explicitly so a future variant that *does* nest one fails to
        // compile here instead of silently bypassing the walk.
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Ref(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Unknown => {}
    }
}

/// Return whether a merged configuration directory is a module definition.
///
/// This is the classification used by the root-owned-block validators: a
/// top-level `arguments` or `attributes` block marks the directory as a
/// module whose provider configuration comes from its caller.
#[must_use]
pub fn is_module_definition<E>(parsed: &crate::parser::File<E>) -> bool {
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
pub fn value_contains_unresolved_ref(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Coalesce { .. })
        | Value::Deferred(DeferredValue::Interpolation(_))
        | Value::Deferred(DeferredValue::FunctionCall { .. })
        | Value::Deferred(DeferredValue::Unavailable { .. })
        | Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
        | Value::Deferred(DeferredValue::Access(_))
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => true,
        Value::Deferred(DeferredValue::Secret(inner)) => value_contains_unresolved_ref(inner),
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(value_contains_unresolved_ref)
        }
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(value_contains_unresolved_ref),
        Value::Concrete(_) => false,
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
        crate::executor::provider_ready::validate_and_canonicalize_provider_config(
            factory.as_ref(),
            &provider.name,
            &provider.attributes,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Validate module call arguments against module argument types.
///
/// `imported_modules` maps each module alias to its resolved boundary signature.
/// `config` provides custom type validators from providers.
pub fn validate_module_calls(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    config: &ProviderContext,
) -> Result<(), String> {
    validate_module_calls_inner(module_calls, imported_modules, config, |_, _| false)
}

/// Validate ordinary module-argument values while deferring binding-owned
/// arguments to [`check_bare_binding_uses`].
pub fn validate_module_calls_with_binding_uses<E>(
    file: &crate::parser::File<E>,
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    config: &ProviderContext,
    binding_use_report: &BindingUseReport,
) -> Result<(), String> {
    validate_module_calls_inner(
        &file.module_calls,
        imported_modules,
        config,
        |call, argument| {
            binding_use_authority_owns_module_argument(
                file,
                binding_use_report,
                call.id(),
                argument,
            )
        },
    )
}

fn validate_module_calls_inner(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    config: &ProviderContext,
    should_skip: impl Fn(&ModuleCall, &str) -> bool,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for call in module_calls {
        if let Some(signature) = imported_modules.get(&call.module_name) {
            let module_args = &signature.arguments;
            let source_file = call.source_file();
            for authored in call.arguments_with_source_spans() {
                let arg_name = authored.name();
                let arg_value = authored.argument();
                let value_span = authored.value_span();
                if let Some(arg_param) = module_args.iter().find(|a| a.name == arg_name) {
                    if should_skip(call, arg_name) {
                        continue;
                    }
                    let line = value_span.start_line();
                    let context = format!(
                        "module {} argument '{}' in {} at line {line}",
                        call.module_name,
                        arg_name,
                        source_file.display(),
                    );
                    let resolved_parameter =
                        match resolve_parameter_type_for_check(&arg_param.type_expr, config) {
                            Ok(Some(resolved)) => resolved,
                            Ok(None) => continue,
                            Err(error) => {
                                errors.push(format!("{context}: {error}"));
                                continue;
                            }
                        };
                    if let Some(handle) = arg_value.handle() {
                        if let Err(error) = check_handle_sink(handle, &resolved_parameter) {
                            errors.push(format!("{context}: {error}"));
                        }
                    } else if arg_value.pending_parameter().is_some() {
                        // A nested call can only classify a forwarded
                        // parameter after its outermost caller is expanded.
                        continue;
                    } else if let Err(error) = typecheck::check_type_expr_value(
                        &resolved_parameter.as_type_expr(),
                        arg_value.value().expect("value module argument"),
                        config,
                        typecheck::TypecheckInference::Unavailable,
                    ) {
                        errors.push(format!("{context}: {error}"));
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
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

fn resolve_and_validate_export_values<P>(
    export_params: &[P],
    bindings: &crate::binding_index::ResolvedBindings,
    config: &ProviderContext,
) -> Result<Vec<P>, String>
where
    P: crate::parser::ExportParamLike + Clone,
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
            match crate::resolver::resolve_ref_value_with_config(value, bindings, config) {
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
                        && let Err(error) = typecheck::check_type_expr_value(
                            type_expr,
                            value,
                            config,
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

/// Resolve every export through the pre-apply value binding graph, evaluate
/// builtins that become concrete, and accumulate both secret-boundary and
/// value-type errors in one pass.
pub fn check_export_values_pre_apply<P>(
    export_params: &[P],
    bindings: &crate::binding_index::ResolvedBindings,
    config: &ProviderContext,
) -> Result<ExportsCheckedPreApply<P>, String>
where
    P: crate::parser::ExportParamLike + Clone,
{
    let display_params = resolve_and_validate_export_values(export_params, bindings, config)?;
    Ok(ExportsCheckedPreApply {
        original_params: export_params.to_vec(),
        display_params,
    })
}

/// Resolve the original export expressions against the post-apply binding
/// view. This is the only constructor for [`CheckedExportValues`], so a
/// pre-apply display snapshot cannot be serialized into state by mistake.
pub fn resolve_export_values_post_apply<P>(
    export_params: &ExportsCheckedPreApply<P>,
    bindings: &crate::binding_index::ResolvedBindings,
    config: &ProviderContext,
) -> Result<CheckedExportValues<P>, String>
where
    P: crate::parser::ExportParamLike + Clone,
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
pub fn validate_attribute_params(
    attribute_params: &[crate::parser::AttributeParameter],
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in attribute_params {
        if let (Some(type_expr), Some(value)) = (&param.type_expr, &param.value)
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
    let mut referenced: HashSet<String> = HashSet::new();
    let mut scoped_bare_references: HashSet<(crate::parser::BindingScopeId, String)> =
        HashSet::new();
    let mut state_block_targets = Vec::new();
    for state_block in &parsed.state_blocks {
        match state_block {
            crate::parser::StateBlock::Import { to, id } => {
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
    let mut defined_bindings: Vec<(String, Option<crate::parser::BindingScopeId>)> = Vec::new();
    for rref in parsed.iter_all_resources() {
        if let Some(binding_name) = rref.binding() {
            if binding_name == "_" {
                continue;
            }
            let scope = parsed
                .parser_binding_metadata
                .resource_source_file(rref.id())
                .map(crate::parser::BindingScopeId::root);
            defined_bindings.push((binding_name.to_string(), scope));
            if matches!(rref, ResourceRef::Resource(_))
                && state_block_targets
                    .iter()
                    .any(|target| target.matches_resource_id(rref.id()))
            {
                referenced.insert(binding_name.to_string());
            }
        }
    }

    if defined_bindings.is_empty() {
        return Vec::new();
    }

    // Collect all referenced binding names. Walk both top-level resources
    // and for-body template resources so bindings referenced only inside a
    // `for` loop are counted as used.
    for rref in parsed.iter_all_resources() {
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
    for deferred in &parsed.deferred_for_expressions {
        collect_dependencies(&deferred.iterable, &mut referenced);
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
        referenced.insert(wb.target.as_str().to_string());
        for dep in &wb.depends_on {
            referenced.insert(dep.as_str().to_string());
        }
    }

    // Return unused binding names, skipping structurally-required bindings
    // (if/for/read expressions) and for-generated indexed bindings (e.g., vpcs[0])
    defined_bindings
        .into_iter()
        .filter(|(binding, scope)| {
            !referenced.contains(binding)
                && !scope.as_ref().is_some_and(|scope| {
                    scoped_bare_references.contains(&(scope.clone(), binding.clone()))
                })
                && !parsed.structural_bindings.contains(binding)
                && !binding.contains('[')
        })
        .map(|(binding, _)| binding)
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
            _ => return Err((current, segment.as_str())),
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
        current = match (current, seg) {
            (TypeExpr::Struct { fields }, PathSegment::Field { name }) => {
                let (_, ty) = fields.into_iter().find(|(n, _)| n == name)?;
                ty
            }
            (TypeExpr::Map(inner), PathSegment::Field { .. }) => *inner,
            (
                TypeExpr::Ref(path) | TypeExpr::DottedUnresolved(path),
                PathSegment::Field { name },
            ) => {
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
            (
                TypeExpr::List(inner),
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
            ) => *inner,
            (
                TypeExpr::Map(inner),
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
            ) => *inner,
            _ => return None,
        };
    }
    Some(current)
}

pub mod inference;

#[cfg(test)]
mod tests;
