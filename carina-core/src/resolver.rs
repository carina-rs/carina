//! Reference resolution for ResourceRef values
//!
//! Resolves `Value::ResourceRef` references by looking up bound resource attributes
//! from both the DSL definition and current infrastructure state.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::binding_index::{BindingValueSource, ResolvedBindings};
use crate::integer_arithmetic::IntegerExpressionError;
#[cfg(test)]
use crate::resource::State;
use crate::resource::{
    AbsoluteBindingAddress, AccessMissPolicy, BindingAddress, ConcreteValue, DataSource,
    DeferredValue, InterpolationPart, LiveComposition, NoPlaceholder, PlaceholderKind,
    PlanInputCurrent, Resource, ResourceId, Value, ValueAccessError, ValuePathSegmentResult,
};

/// Failure to evaluate a reference after selecting its historical input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionError {
    #[error("{}", crate::resource::INSIDE_SECRET_MESSAGE)]
    InsideSecret {},
    #[error(transparent)]
    UnavailableSecret(#[from] crate::resource::UnavailableSecretError),
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Knowledge(Box<crate::resource::knowledge::KnowledgeDiagnostic>),
    #[error(transparent)]
    AdmissionConstraints(Box<crate::validation::admission::AdmissionFindings>),
    #[error("{context}{error}")]
    Context {
        context: String,
        error: Box<ResolutionError>,
    },
}

impl ResolutionError {
    pub(crate) fn with_context(self, context: impl Into<String>) -> Self {
        let context = context.into();
        match self {
            Self::Message(message) => Self::Message(format!("{context}{message}")),
            error => Self::Context {
                context,
                error: Box::new(error),
            },
        }
    }
}

impl<B: BindingAddress> From<crate::resource::knowledge::KnowledgeError<B>> for ResolutionError {
    fn from(error: crate::resource::knowledge::KnowledgeError<B>) -> Self {
        match error {
            crate::resource::knowledge::KnowledgeError::UnavailableHistoricalSecret(error) => {
                Self::UnavailableSecret(error)
            }
            error => Self::Knowledge(Box::new(error.into())),
        }
    }
}

impl crate::resource::InsideSecret for ResolutionError {
    fn inside_secret(self) -> Self {
        match self {
            Self::UnavailableSecret(error) => Self::UnavailableSecret(error.inside_secret()),
            Self::AdmissionConstraints(findings) => {
                Self::AdmissionConstraints(Box::new((*findings).inside_secret()))
            }
            _ => Self::InsideSecret {},
        }
    }
}

impl From<&str> for ResolutionError {
    fn from(message: &str) -> Self {
        Self::Message(message.to_owned())
    }
}

impl From<String> for ResolutionError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<ValueAccessError> for ResolutionError {
    fn from(error: ValueAccessError) -> Self {
        match error {
            ValueAccessError::Classification { error } => *error,
            error => Self::Message(error.to_string()),
        }
    }
}

impl From<crate::parser::ParseError> for ResolutionError {
    fn from(error: crate::parser::ParseError) -> Self {
        match error {
            crate::parser::ParseError::UnavailableSecret(error) => Self::UnavailableSecret(error),
            crate::parser::ParseError::InsideSecret { .. } => Self::InsideSecret {},
            crate::parser::ParseError::Evaluation { line, error } => {
                error.with_context(format!("Invalid expression at line {line}: "))
            }
            crate::parser::ParseError::Located { path, error } => match Self::from(*error) {
                Self::Message(message) => Self::Message(format!("{}: {message}", path.display())),
                error @ (Self::Knowledge(_)
                | Self::AdmissionConstraints(_)
                | Self::Context { .. }) => error.with_context(format!("{}: ", path.display())),
                unavailable @ (Self::UnavailableSecret(_) | Self::InsideSecret { .. }) => {
                    unavailable
                }
            },
            crate::parser::ParseError::AtElement(finding) => {
                let (location, error) = (*finding).into_parts();
                match Self::from(error) {
                    Self::Message(message) => {
                        Self::Message(crate::parser::Located::new(location, message).to_string())
                    }
                    error @ (Self::Knowledge(_)
                    | Self::AdmissionConstraints(_)
                    | Self::Context { .. }) => {
                        error.with_context(crate::parser::Located::new(location, "").to_string())
                    }
                    unavailable @ (Self::UnavailableSecret(_) | Self::InsideSecret { .. }) => {
                        unavailable
                    }
                }
            }
            error => Self::Message(error.to_string()),
        }
    }
}

impl From<crate::resource::SnapshotSelectionError> for ResolutionError {
    fn from(error: crate::resource::SnapshotSelectionError) -> Self {
        match error {
            crate::resource::SnapshotSelectionError::UnavailableSecret(error) => error.into(),
            crate::resource::SnapshotSelectionError::Access(error)
            | crate::resource::SnapshotSelectionError::DeferredMiss(error) => error.into(),
        }
    }
}

/// Resolve all ResourceRef values in resources using current state.
///
/// Builds a binding map from resource attributes and state, then resolves
/// all ResourceRef values across all resources.
///
/// Before resolving, saves dependency binding names in the typed
/// `Resource::dependency_bindings` field. This preserves dependency information
/// that would otherwise be lost when ResourceRef values are replaced with plain
/// strings.
///
/// `remote_bindings` provides external bindings from upstream state data sources.
/// Each entry maps an upstream_state binding name to a map of resource binding names
/// to their attributes. For example, `network -> { vpc -> { vpc_id -> "vpc-123" } }`.
pub fn resolve_refs_with_state(
    resources: &mut [Resource],
    managed_current: &HashMap<ResourceId, PlanInputCurrent>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    let bindings = crate::binding_index::ResolvedBindings::pre_apply_live(
        crate::binding_index::LivePreApplyInputs {
            managed: resources,
            compositions: &[],
            managed_current,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    );
    resolve_refs_with_state_and_remote(resources, &bindings, config)
}

/// Resolve all `ResourceRef` values in `resources` against a
/// pre-built `ResolvedBindings` view. carina#3248: the caller is
/// responsible for assembling the bindings view via
/// [`ResolvedBindings::pre_apply`], so this entry point is a pure
/// transform that takes the view as input.
///
/// Use this on the apply path. Use [`resolve_refs_for_plan`] on the
/// plan path — it additionally stamps any surviving
/// `ResourceRef` whose root binding is named in
/// `bindings_upstream_keys` as
/// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))` so plan
/// display can render it as `(known after upstream apply: <ref>)`
/// instead of the raw dot-form.
pub fn resolve_refs_with_state_and_remote(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    resolve_refs_inner(
        resources,
        bindings,
        &std::collections::HashSet::new(),
        config,
    )
}

/// Plan-only counterpart of [`resolve_refs_with_state_and_remote`].
/// Same input/output shape (pre-built `ResolvedBindings`), with the
/// addition of `unresolved_upstream_bindings`: the set of upstream-
/// state binding names whose surviving refs should be stamped as
/// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))` for display.
/// `apply` continues to call the strict variant. See #2366 / RFC #2371.
pub fn resolve_refs_for_plan(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    resolve_refs_inner(resources, bindings, unresolved_upstream_bindings, config)
}

fn resolve_refs_inner(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    // Save dependency bindings before resolution destroys ResourceRef values.
    // This metadata is used by plan tree building to recover parent-child
    // relationships (see build_plan_tree in display.rs and app.rs).
    for resource in resources.iter_mut() {
        let deps = crate::deps::get_resource_value_ref_dependencies(crate::parser::ResourceRef::<
            '_,
            crate::resource::ModuleSource,
            NoPlaceholder,
            NoPlaceholder,
            crate::parser::ResolvedDeferredForPhase<
                NoPlaceholder,
                NoPlaceholder,
                AbsoluteBindingAddress,
            >,
            AbsoluteBindingAddress,
        >::Resource(resource));
        if !deps.is_empty() {
            resource.dependencies.value_bindings = deps.into_iter().collect();
        }
    }

    let mark_unresolved_upstream = !unresolved_upstream_bindings.is_empty();

    // Resolve ResourceRef values in all resources. Stay in `IndexMap`
    // so the user's authored attribute order survives resolution
    // (#2222).
    for resource in resources.iter_mut() {
        let mut resolved_attrs: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        for (key, value) in &resource.attributes {
            let resolved = resolve_ref_value(value, bindings, config)?;
            let final_value = if mark_unresolved_upstream {
                stamp_unresolved_upstream(resolved, unresolved_upstream_bindings)
                    .map_err(|error| error.to_string())?
            } else {
                resolved
            };
            resolved_attrs.insert(key.clone(), final_value);
        }
        resource.attributes = resolved_attrs;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Typed resolver entry points (#3175 / typestate split)
// ---------------------------------------------------------------------------

/// Pre-apply path: resolve `ResourceRef` values across a slice of
/// `Resource`s.
///
/// carina#3248: compositions and data sources are first-class binding
/// sources on the pre-apply path. `bindings` must therefore include
/// all kinds the configuration declares, via
/// [`ResolvedBindings::pre_apply`]. A managed attribute referencing
/// `<module_instance>.<attr>` (a composition binding) chains through the
/// composition's attribute map to the managed sibling literal that backs
/// it. The earlier "wait / upstream-state passthrough" guidance for
/// this shape (referring to cross-stack consumption) does not apply
/// to same-stack module attribute references — those resolve through
/// the in-process composition binding directly.
pub fn resolve_managed_refs_with_state_and_remote(
    managed: &mut [Resource],
    bindings: &ResolvedBindings,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    resolve_refs_inner(managed, bindings, &std::collections::HashSet::new(), config)
}

/// Resolve `ResourceRef` values in a slice of [`DataSource`]s against
/// a pre-built `ResolvedBindings` view (carina#3248).
///
/// Data sources are read-only; their input attributes (`read aws.iam.user
/// { user_name = some_let.name }`) reference managed resources, compositions,
/// or other data sources, so the binding map the caller passes must
/// include all kinds via [`ResolvedBindings::pre_apply`]. Each data
/// source's `dependency_bindings` is recorded before resolution
/// destroys the `ResourceRef` values, mirroring [`resolve_refs_inner`].
pub fn resolve_data_source_refs(
    data_sources: &mut [DataSource],
    bindings: &ResolvedBindings,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    resolve_data_source_refs_inner(
        data_sources,
        bindings,
        &std::collections::HashSet::new(),
        config,
    )
}

/// Plan-only counterpart of [`resolve_data_source_refs`]. Mirrors
/// [`resolve_refs_for_plan`]: takes the set of upstream-binding
/// names whose surviving refs should be stamped as
/// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))`.
pub fn resolve_data_source_refs_for_plan(
    data_sources: &mut [DataSource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    resolve_data_source_refs_inner(data_sources, bindings, unresolved_upstream_bindings, config)
}

fn resolve_data_source_refs_inner(
    data_sources: &mut [DataSource],
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    for data_source in data_sources.iter_mut() {
        let deps =
            crate::deps::get_resource_value_ref_dependencies(crate::parser::ResourceRef::<
                '_,
                crate::resource::ModuleSource,
                NoPlaceholder,
                NoPlaceholder,
                crate::parser::ResolvedDeferredForPhase<
                    NoPlaceholder,
                    NoPlaceholder,
                    AbsoluteBindingAddress,
                >,
                AbsoluteBindingAddress,
            >::DataSource(data_source));
        if !deps.is_empty() {
            data_source.dependencies.value_bindings = deps.into_iter().collect();
        }
    }
    let mark_unresolved_upstream = !unresolved_upstream_bindings.is_empty();
    for data_source in data_sources.iter_mut() {
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();
        for (key, value) in &data_source.attributes {
            let resolved = resolve_ref_value(value, bindings, config)?;
            let final_value = if mark_unresolved_upstream {
                stamp_unresolved_upstream(resolved, unresolved_upstream_bindings)
                    .map_err(|error| error.to_string())?
            } else {
                resolved
            };
            resolved_attrs.insert(key.clone(), final_value);
        }
        data_source.attributes = resolved_attrs;
    }
    Ok(())
}

/// Post-apply path: resolve `ResourceRef` values across a slice of
/// `LiveComposition`s using a `ResolvedBindings` view that the caller
/// has already built against the post-apply state.
///
/// Calling this against pre-apply state would re-introduce the
/// #3169 exports-drift bug, so the caller is responsible for the
/// post-apply ordering. The signature is structurally distinct from
/// [`resolve_managed_refs_with_state_and_remote`] (takes a built
/// bindings view, not the raw state inputs) to make accidental
/// pre-apply use harder to write.
///
/// `dependency_bindings` is left untouched: composition→resource edges are
/// already recorded during the pre-apply pass, and the post-apply
/// resolution only needs to materialise the attribute values.
pub fn resolve_live_composition_refs_post_apply(
    compositions: &mut [LiveComposition],
    bindings: &ResolvedBindings,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    for v in compositions.iter_mut() {
        v.signature.attributes =
            resolve_composition_attributes(&v.signature.attributes, bindings, config)?;
    }
    Ok(())
}

pub fn resolve_saved_composition_refs_post_apply(
    compositions: &mut [crate::resource::SavedComposition<crate::resource::NoPlaceholder>],
    bindings: &ResolvedBindings,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ResolutionError> {
    for composition in compositions {
        composition.signature.attributes =
            resolve_composition_attributes(&composition.signature.attributes, bindings, config)?;
    }
    Ok(())
}

trait RuntimeCompositionAttribute: Sized + Clone {
    fn runtime_value(&self) -> Option<Value>;
    fn with_runtime_value(&self, value: Value) -> Self;
}

impl RuntimeCompositionAttribute for crate::resource::LiveCompositionAttribute {
    fn runtime_value(&self) -> Option<Value> {
        self.value()
    }

    fn with_runtime_value(&self, value: Value) -> Self {
        self.with_value(value)
    }
}

impl RuntimeCompositionAttribute
    for crate::resource::SavedCompositionAttribute<crate::resource::NoPlaceholder>
{
    fn runtime_value(&self) -> Option<Value> {
        Some(self.to_value())
    }

    fn with_runtime_value(&self, value: Value) -> Self {
        self.with_value(value)
    }
}

fn resolve_composition_attributes<A: RuntimeCompositionAttribute>(
    attributes: &IndexMap<String, A>,
    bindings: &ResolvedBindings,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<IndexMap<String, A>, ResolutionError> {
    attributes
        .iter()
        .map(|(key, attribute)| {
            // Reify the typed attribute into a value, resolve it, then
            // reclassify it in the same lifecycle. A forwarded reference that
            // resolves to a literal becomes derived; one that remains a
            // reference stays forwarded.
            let Some(value) = attribute.runtime_value() else {
                return Ok((key.clone(), attribute.clone()));
            };
            let resolved = resolve_ref_value(&value, bindings, config)?;
            Ok((key.clone(), attribute.with_runtime_value(resolved)))
        })
        .collect()
}

/// Replace any `Value::ResourceRef` whose root binding is in
/// `upstream_binding_names` with the unresolved-upstream marker string.
/// Recurses through `List` / `Map` / `Interpolation` / `FunctionCall` /
/// `Secret` so nested upstream refs (e.g. inside a `tags` map) are also
/// stamped. Subtrees containing no upstream refs are returned by move
/// without rebuilding.
fn stamp_unresolved_upstream(
    value: Value,
    upstream_binding_names: &std::collections::HashSet<&str>,
) -> Result<Value, IntegerExpressionError> {
    Ok(match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let deferred = deferred.try_map(|value| {
                stamp_unresolved_upstream(value.clone(), upstream_binding_names)
            })?;
            Value::Deferred(DeferredValue::ModuleIf(Box::new(deferred)))
        }
        Value::Deferred(DeferredValue::ResourceRef { path })
            if upstream_binding_names.contains(path.binding()) =>
        {
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamExport { path },
            ))
        }
        Value::Deferred(DeferredValue::BindingRef { binding })
            if upstream_binding_names.contains(binding.as_str()) =>
        {
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports { binding },
            ))
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(stamp_unresolved_upstream(*value, upstream_binding_names)?),
                default: Box::new(stamp_unresolved_upstream(*default, upstream_binding_names)?),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = stamp_unresolved_upstream(expression.lhs().clone(), upstream_binding_names)?;
            let rhs = stamp_unresolved_upstream(expression.rhs().clone(), upstream_binding_names)?;
            expression.evaluate_with_operands(lhs, rhs)?
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .into_iter()
                .map(|v| stamp_unresolved_upstream(v, upstream_binding_names))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut out: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                out.insert(k, stamp_unresolved_upstream(v, upstream_binding_names)?);
            }
            Value::Concrete(ConcreteValue::Map(out))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .into_iter()
                    .map(|p| match p {
                        InterpolationPart::Expr(v) => {
                            stamp_unresolved_upstream(v, upstream_binding_names)
                                .map(InterpolationPart::Expr)
                        }
                        other => Ok(other),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::FunctionCall {
            name,
            args: args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        stamp_unresolved_upstream(value.clone(), upstream_binding_names)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            source_line,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args: args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        stamp_unresolved_upstream(value.clone(), upstream_binding_names)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            source_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => {
            let original = Value::Deferred(DeferredValue::Access(access.clone()));
            access
                .rebuild_with_base(
                    stamp_unresolved_upstream(access.base().clone(), upstream_binding_names)?,
                    AccessMissPolicy::Error,
                )
                .unwrap_or(original)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            Value::Deferred(DeferredValue::Secret(inner.try_map(|value| {
                stamp_unresolved_upstream(value.clone(), upstream_binding_names)
            })?))
        }
        // Already-stamped or non-reference leaves pass through unchanged.
        value @ Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | value @ Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::Unknown(_),
        ) => value,
        Value::Deferred(DeferredValue::Placeholder(never)) => match never {},
    })
}

/// Plan-only single-value counterpart of [`resolve_refs_for_plan`].
///
/// Resolves `ResourceRef` segments against `bindings` and then stamps
/// any surviving ref whose root binding is in
/// `unresolved_upstream_bindings` as
/// `Value::Deferred(DeferredValue::Unknown(UpstreamExport { path }))` so
/// plan display renders it as `(known after upstream apply: …)`.
///
/// Used by `carina_cli::wiring::add_state_block_effects` for the `id`
/// value of an `import { … }` block: a `"${X.attr}|…"`
/// interpolation that references a deferred upstream-state value must
/// either fold to a concrete cloud identifier (when `X.attr` is in
/// scope) or carry the deferred marker through plan display (carina#3329).
/// Failed live expressions retain their authored form for plan display.
/// Selecting an unavailable historical secret returns its typed error.
pub fn resolve_value_for_plan(
    value: &Value,
    bindings: &ResolvedBindings,
    unresolved_upstream_bindings: &std::collections::HashSet<&str>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Value, ResolutionError> {
    let resolved = match resolve_ref_value(value, bindings, config) {
        Ok(value) => value,
        Err(error @ ResolutionError::UnavailableSecret(_)) => return Err(error),
        Err(
            ResolutionError::Message(_)
            | ResolutionError::InsideSecret { .. }
            | ResolutionError::Knowledge(_)
            | ResolutionError::AdmissionConstraints(_)
            | ResolutionError::Context { .. },
        ) => value.clone(),
    };
    Ok(if unresolved_upstream_bindings.is_empty() {
        resolved
    } else {
        stamp_unresolved_upstream(resolved, unresolved_upstream_bindings)
            .unwrap_or_else(|_| value.clone())
    })
}

/// Recursively resolve a single Value, replacing ResourceRef with the referenced value.
///
/// If the referenced binding or attribute is not found, the value is returned as-is.
/// Returns an error if a builtin function fails with fully-resolved arguments.
pub fn resolve_ref_value<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Value<P, B>, ResolutionError> {
    resolve_ref_value_inner(value, bindings, RuntimeContext::Session(config))
}

/// Project binding data without evaluating user functions. Their annotated
/// call nodes remain values that must be checked by a runtime session.
pub(crate) fn project_ref_value_status<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    context: &crate::parser::ProviderContext,
) -> Result<ValueResolution<P, B>, ResolutionError> {
    resolve_ref_value_status_inner(value, bindings, RuntimeContext::Projection(context), false)
}

#[derive(Clone, Copy)]
pub(crate) enum RuntimeContext<'a> {
    Session(&'a dyn crate::validation::deferred_types::ProviderTypeCheckContext),
    Projection(&'a crate::parser::ProviderContext),
}

impl<'a> RuntimeContext<'a> {
    fn catalog(self) -> &'a crate::parser::ProviderContext {
        match self {
            Self::Session(session) => session.catalog(),
            Self::Projection(context) => context,
        }
    }
}

/// Three-state result used by null/missing coalescing. A missing value is not
/// interchangeable with a value whose producing snapshot is still incomplete.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueResolution<
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    Present(Value<P, B>),
    Absent(crate::resource::knowledge::AbsenceWitness<B>),
    Unknown(Value<P, B>),
}

/// The single plan-time reference decision. Consumers supply expression
/// evaluation, but binding definitiveness and selection are decided here.
pub(crate) enum PlanReferenceResolution<P: PlaceholderKind, B: BindingAddress> {
    Present(Value<P, B>),
    Pending(crate::resource::knowledge::Phase),
    Absent(crate::resource::knowledge::AbsenceWitness<B>),
    SelectionError {
        error: ValueAccessError,
        absence_is_definitive: bool,
    },
}
pub(crate) enum ReferenceEvaluation<P: PlaceholderKind, B: BindingAddress> {
    Known(Value<P, B>),
    Pending(crate::resource::knowledge::Phase),
}
pub(crate) enum ReferenceError<E> {
    Evaluation(E),
    Historical(crate::resource::UnavailableSecretError),
}

pub(crate) fn plan_reference_status<P: PlaceholderKind, B: BindingAddress, E>(
    binding: &B,
    selection: Option<&crate::resource::AccessPath<B>>,
    bindings: &ResolvedBindings<P, B>,
    mut evaluate: impl FnMut(Value<P, B>, bool) -> Result<ReferenceEvaluation<P, B>, E>,
) -> Result<PlanReferenceResolution<P, B>, ReferenceError<E>> {
    use crate::resource::knowledge::{AbsenceWitness, Phase};
    let pending = || PlanReferenceResolution::Pending(Phase::Apply);
    let Some(attributes) = bindings.get(binding.as_str()) else {
        return Ok(pending());
    };
    let Some(path) = selection else {
        if !bindings.binding_is_fully_definitive(binding.as_str()) {
            return Ok(pending());
        }
        let mut keys = attributes.keys().collect::<Vec<_>>();
        keys.sort();
        let values = keys
            .into_iter()
            .map(|key| {
                attributes[key]
                    .to_value_at(format!("{binding}.{key}"))
                    .map(|value| (key.clone(), value))
            })
            .collect::<Result<_, _>>()
            .map_err(ReferenceError::Historical)?;
        return evaluate(Value::Concrete(ConcreteValue::Map(values)), false)
            .map(|read| match read {
                ReferenceEvaluation::Known(value) => PlanReferenceResolution::Present(value),
                ReferenceEvaluation::Pending(phase) => PlanReferenceResolution::Pending(phase),
            })
            .map_err(ReferenceError::Evaluation);
    };
    let Some(attribute) = attributes.get(path.attribute()) else {
        return Ok(match AbsenceWitness::binding_miss(path, bindings) {
            Some(witness) => PlanReferenceResolution::Absent(witness),
            None => pending(),
        });
    };
    if !bindings.attribute_value_is_definitive(binding.as_str(), path.attribute()) {
        return Ok(pending());
    }
    let absence_is_definitive =
        bindings.attribute_absence_is_definitive(binding.as_str(), path.attribute());
    let selection_error = |error| PlanReferenceResolution::SelectionError {
        error,
        absence_is_definitive,
    };
    let root_path = format!("{}.{}", path.binding(), path.attribute());
    let mut selected_segments = 0;
    let selected = loop {
        match attribute.select_path(
            &path.segments()[..selected_segments],
            &root_path,
            AccessMissPolicy::Error,
        ) {
            Ok(value) => break value,
            Err(crate::resource::SnapshotSelectionError::UnavailableSecret(_))
                if selected_segments < path.segments().len() =>
            {
                selected_segments += 1
            }
            Err(crate::resource::SnapshotSelectionError::UnavailableSecret(error)) => {
                return Err(ReferenceError::Historical(error));
            }
            Err(crate::resource::SnapshotSelectionError::Access(error)) => {
                return Ok(selection_error(error));
            }
            Err(crate::resource::SnapshotSelectionError::DeferredMiss(_)) => return Ok(pending()),
        }
    };
    let mut selected = match evaluate(selected, true).map_err(ReferenceError::Evaluation)? {
        ReferenceEvaluation::Known(value) => value,
        ReferenceEvaluation::Pending(phase) => return Ok(PlanReferenceResolution::Pending(phase)),
    };
    // The root was admitted once. Selecting its children only needs the shallow
    // access operation; it cannot introduce an unevaluated sibling.
    for segment in &path.segments()[selected_segments..] {
        selected = match crate::resource::apply_value_path_segment(
            selected,
            segment.clone(),
            0,
            AccessMissPolicy::Error,
        ) {
            Ok(ValuePathSegmentResult::Resolved(value)) => value,
            Ok(ValuePathSegmentResult::Deferred(_) | ValuePathSegmentResult::DeferredMiss(_)) => {
                return Ok(pending());
            }
            Err(error) => return Ok(selection_error(error)),
        };
    }
    Ok(PlanReferenceResolution::Present(selected))
}

/// Resolve a value while preserving the distinction required by `??`.
pub fn resolve_ref_value_status<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<ValueResolution<P, B>, ResolutionError> {
    resolve_ref_value_status_inner(value, bindings, RuntimeContext::Session(config), false)
}

/// Resolve a value using binding data and a provider-aware builtin context.
///
/// Export validation uses this entry point because a reference may become an
/// argument to `decrypt()` (or another context-sensitive builtin) only after
/// its binding has been substituted.
pub fn resolve_ref_value_with_config<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Value<P, B>, ResolutionError> {
    resolve_ref_value_inner(value, bindings, RuntimeContext::Session(config))
}

/// Select the miss policy for a path-shaped reference. A missing source is not
/// evidence of a partially-known local value, so it uses the strict policy.
fn binding_access_miss_policy<P: PlaceholderKind, B: BindingAddress>(
    binding: &str,
    bindings: &ResolvedBindings<P, B>,
) -> AccessMissPolicy {
    let Some(source) = bindings.source(binding) else {
        return AccessMissPolicy::Error;
    };
    match source {
        BindingValueSource::Local => AccessMissPolicy::Defer,
        BindingValueSource::Upstream => AccessMissPolicy::Error,
        BindingValueSource::WaitAlias { .. } => AccessMissPolicy::Defer,
    }
}

enum HistoricalSelectionError {
    Access(ValueAccessError),
    Resolution(ResolutionError),
}

/// Resolve ordinary expressions before applying access steps, as for live
/// bindings. If a hash blocks whole-attribute conversion, select the requested
/// subtree first so unrelated historical evidence remains opaque.
fn resolve_historical_selection<P: PlaceholderKind, B: BindingAddress>(
    attribute: &crate::resource::SnapshotValue<P, B>,
    path: &crate::resource::AccessPath<B>,
    bindings: &ResolvedBindings<P, B>,
    config: RuntimeContext<'_>,
    policy: AccessMissPolicy,
) -> Result<Option<Value<P, B>>, HistoricalSelectionError> {
    let root_path = format!("{}.{}", path.binding(), path.attribute());
    // Find the first selectable subtree that can cross the checked conversion.
    // Resolve its live expressions before applying the remaining access steps.
    // In particular, a hash sibling must not change deferral behavior beneath
    // an ordinary child selected from the same historical container.
    let mut selected_segments = 0;
    let selected = loop {
        match attribute.select_path(&path.segments()[..selected_segments], &root_path, policy) {
            Ok(value) => break value,
            Err(crate::resource::SnapshotSelectionError::UnavailableSecret(_))
                if selected_segments < path.segments().len() =>
            {
                selected_segments += 1;
            }
            Err(crate::resource::SnapshotSelectionError::UnavailableSecret(error)) => {
                return Err(HistoricalSelectionError::Resolution(error.into()));
            }
            Err(crate::resource::SnapshotSelectionError::Access(error)) => {
                return Err(HistoricalSelectionError::Access(error));
            }
            Err(crate::resource::SnapshotSelectionError::DeferredMiss(_)) => return Ok(None),
        }
    };
    let mut resolved = resolve_ref_value_inner(&selected, bindings, config)
        .map_err(HistoricalSelectionError::Resolution)?;
    for segment in &path.segments()[selected_segments..] {
        resolved =
            match crate::resource::apply_value_path_segment(resolved, segment.clone(), 0, policy) {
                Ok(ValuePathSegmentResult::Resolved(next)) => {
                    resolve_ref_value_inner(&next, bindings, config)
                        .map_err(HistoricalSelectionError::Resolution)?
                }
                Ok(
                    ValuePathSegmentResult::Deferred(_) | ValuePathSegmentResult::DeferredMiss(_),
                ) => {
                    return Ok(None);
                }
                Err(error) => return Err(HistoricalSelectionError::Access(error)),
            };
    }
    Ok(Some(resolved))
}

fn resolve_plan_reference<P: PlaceholderKind, B: BindingAddress>(
    original: &Value<P, B>,
    binding: &B,
    selection: Option<&crate::resource::AccessPath<B>>,
    bindings: &ResolvedBindings<P, B>,
    config: RuntimeContext<'_>,
) -> Result<ValueResolution<P, B>, ResolutionError> {
    use crate::resource::knowledge::{Classification, KnowledgeContext, Phase};
    let status = plan_reference_status(binding, selection, bindings, |value, resolve| {
        let value = if resolve {
            resolve_ref_value_inner(&value, bindings, config)?
        } else {
            value
        };
        let classification = value
            .knowledge(&KnowledgeContext::plan(bindings))
            .map(|proof| proof.into_classification())?;
        let phase = if classification.apply_required() {
            Phase::Apply
        } else {
            Phase::Plan
        };
        let known = matches!(classification, Classification::Known(_));
        if let Some(error) = classification.failure() {
            return Err(ResolutionError::from(error));
        }
        Ok(if known {
            ReferenceEvaluation::Known(value)
        } else {
            ReferenceEvaluation::Pending(phase)
        })
    })
    .map_err(|error| match error {
        ReferenceError::Evaluation(error) => error,
        ReferenceError::Historical(error) => error.into(),
    })?;
    match status {
        PlanReferenceResolution::Present(value) => Ok(ValueResolution::Present(value)),
        PlanReferenceResolution::Pending(_) => Ok(ValueResolution::Unknown(original.clone())),
        PlanReferenceResolution::Absent(witness) => Ok(ValueResolution::Absent(witness)),
        PlanReferenceResolution::SelectionError {
            error,
            absence_is_definitive,
        } => match crate::resource::knowledge::AbsenceWitness::selection_miss(&error) {
            Some(witness) if absence_is_definitive => Ok(ValueResolution::Absent(witness)),
            Some(_) => Ok(ValueResolution::Unknown(original.clone())),
            None => Err(error
                .format_for_path(
                    &selection
                        .map(ToString::to_string)
                        .unwrap_or_else(|| binding.to_string()),
                )
                .into()),
        },
    }
}

fn resolve_ref_value_status_inner<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    config: RuntimeContext<'_>,
    allow_absent_default: bool,
) -> Result<ValueResolution<P, B>, ResolutionError> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => Err(
            "unresolved module-template conditional reached runtime resolver"
                .to_string()
                .into(),
        ),
        Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            let left_resolution = if let Some(operand) = left.optional_operand() {
                resolve_ref_value_status_inner(operand.value(), bindings, config, true)?
            } else {
                resolve_required_value_status(left, bindings, config)?
            };
            match left_resolution {
                ValueResolution::Present(selected) => {
                    match selected
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                            bindings,
                        ))
                        .map(|proof| proof.into_classification())
                        .map_err(ResolutionError::from)?
                    {
                        crate::resource::knowledge::Classification::Known(proof) => {
                            Ok(ValueResolution::Present(
                                crate::resource::knowledge::take_present(proof),
                            ))
                        }
                        crate::resource::knowledge::Classification::Failed(value) => {
                            Err(value.error().into())
                        }
                        crate::resource::knowledge::Classification::ShapeKnown(_)
                        | crate::resource::knowledge::Classification::Pending(_) => Ok(
                            ValueResolution::Unknown(Value::Deferred(DeferredValue::Coalesce {
                                value: Box::new(selected),
                                default: default.clone(),
                            })),
                        ),
                    }
                }
                ValueResolution::Absent(_witness)
                    if allow_absent_default && default.optional_operand().is_some() =>
                {
                    resolve_ref_value_status_inner(default, bindings, config, true)
                }
                ValueResolution::Absent(witness) => {
                    crate::resource::knowledge::take_fallback(witness, || {
                        match resolve_required_value_status(default, bindings, config)? {
                            ValueResolution::Present(value) | ValueResolution::Unknown(value) => {
                                Ok(value)
                            }
                            ValueResolution::Absent(_) => {
                                unreachable!("required resolution cannot produce absence")
                            }
                        }
                    })
                    .and_then(|selected| {
                        match selected
                            .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                                bindings,
                            ))
                            .map(|proof| proof.into_classification())
                        {
                            Ok(crate::resource::knowledge::Classification::Known(_)) => {
                                Ok(ValueResolution::Present(selected))
                            }
                            Ok(
                                crate::resource::knowledge::Classification::ShapeKnown(_)
                                | crate::resource::knowledge::Classification::Pending(_),
                            ) => Ok(ValueResolution::Unknown(selected)),
                            Ok(crate::resource::knowledge::Classification::Failed(value)) => {
                                Err(value.error().into())
                            }
                            Err(error) => Err(error.into()),
                        }
                    })
                }
                ValueResolution::Unknown(_) => {
                    // Value presence still requires Known. A definitive
                    // reference can nevertheless expose collection membership:
                    // retain that classified shape inside the unresolved
                    // coalesce for the PlanIterable consumer, without selecting
                    // either operand or granting a known-value proof.
                    let shape = match left.as_ref() {
                        Value::Deferred(DeferredValue::ResourceRef { path })
                            if bindings.attribute_value_is_definitive(
                                path.binding(),
                                path.attribute(),
                            ) =>
                        {
                            let selected = resolve_ref_value_inner(left, bindings, config)?;
                            match selected
                                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                                    bindings,
                                ))
                                .map(|proof| proof.into_classification())
                                .map_err(ResolutionError::from)?
                            {
                                crate::resource::knowledge::Classification::ShapeKnown(shape) => {
                                    Some(shape.to_value())
                                }
                                crate::resource::knowledge::Classification::Known(_)
                                | crate::resource::knowledge::Classification::Pending(_)
                                | crate::resource::knowledge::Classification::Failed(_) => None,
                            }
                        }
                        Value::Deferred(DeferredValue::Secret(_)) => None,
                        Value::Concrete(_)
                        | Value::Deferred(
                            DeferredValue::ModuleIf(_)
                            | DeferredValue::Interpolation(_)
                            | DeferredValue::IntegerArithmetic(_)
                            | DeferredValue::FunctionCall { .. }
                            | DeferredValue::ResolvedFunctionCall { .. }
                            | DeferredValue::BindingRef { .. }
                            | DeferredValue::Access(_)
                            | DeferredValue::Unknown(_)
                            | DeferredValue::Placeholder(_)
                            | DeferredValue::Unavailable { .. }
                            | DeferredValue::Coalesce { .. }
                            | DeferredValue::ResourceRef { .. },
                        ) => None,
                    };
                    Ok(ValueResolution::Unknown(match shape {
                        Some(shape) => Value::Deferred(DeferredValue::Coalesce {
                            value: Box::new(shape),
                            default: default.clone(),
                        }),
                        None => value.clone(),
                    }))
                }
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            resolve_plan_reference(value, path.binding_address(), Some(path), bindings, config)
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            resolve_plan_reference(value, binding, None, bindings, config)
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let resolved_base = match resolve_ref_value_status_inner(
                access.base(),
                bindings,
                config,
                allow_absent_default,
            )? {
                ValueResolution::Present(value) => value,
                ValueResolution::Absent(witness) => return Ok(ValueResolution::Absent(witness)),
                ValueResolution::Unknown(_) => {
                    return Ok(ValueResolution::Unknown(value.clone()));
                }
            };
            match access.rebuild_result(
                resolved_base,
                access.step().clone(),
                AccessMissPolicy::Error,
            ) {
                Ok(ValuePathSegmentResult::Resolved(value)) => {
                    resolve_ref_value_status_inner(&value, bindings, config, allow_absent_default)
                }
                Ok(
                    ValuePathSegmentResult::Deferred(_) | ValuePathSegmentResult::DeferredMiss(_),
                ) => Ok(ValueResolution::Unknown(value.clone())),
                Err(error) => {
                    match crate::resource::knowledge::AbsenceWitness::selection_miss(&error) {
                        Some(witness) => Ok(ValueResolution::Absent(witness)),
                        None => Err(error
                            .format_for_path(&crate::value::format_value(value))
                            .into()),
                    }
                }
            }
        }
        Value::Deferred(DeferredValue::Unknown(_)) => Ok(ValueResolution::Unknown(value.clone())),
        Value::Deferred(DeferredValue::Unavailable { name, reason, .. }) => {
            Err(format!("{name}(): {reason}").into())
        }
        Value::Concrete(_)
        | Value::Deferred(
            DeferredValue::Interpolation(_)
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Secret(_),
        ) => {
            let resolved = resolve_ref_value_inner(value, bindings, config)?;
            if !resolved
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .is_ok_and(|value| value.into_known().is_ok())
            {
                Ok(ValueResolution::Unknown(resolved))
            } else {
                Ok(ValueResolution::Present(resolved))
            }
        }
        Value::Deferred(DeferredValue::Placeholder(_)) => {
            Ok(ValueResolution::Unknown(value.clone()))
        }
    }
}

/// Resolve a required expression without interpreting a direct miss as the
/// absence of an enclosing coalesce operand.
fn resolve_required_value_status<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    config: RuntimeContext<'_>,
) -> Result<ValueResolution<P, B>, ResolutionError> {
    let resolved = resolve_ref_value_inner(value, bindings, config)?;
    if resolved
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .is_ok_and(|value| value.into_known().is_ok())
    {
        Ok(ValueResolution::Present(resolved))
    } else {
        Ok(ValueResolution::Unknown(resolved))
    }
}

fn locate_user_function_error<B: BindingAddress>(
    function: &crate::parser::UserFunction<B>,
    error: crate::parser::ParseError,
) -> crate::parser::ParseError {
    match &function.body.source_file {
        Some(path) => error.in_file(path.clone()),
        None => error,
    }
}

fn user_function_runtime_error(
    line: usize,
    error: impl Into<ResolutionError>,
) -> crate::parser::ParseError {
    crate::parser::ParseError::from_resolution(error.into(), line)
}

fn function_annotation_site<B: BindingAddress>(
    function: &crate::parser::UserFunction<B>,
    kind: crate::validation::TypeResolutionDeclarationKind,
    name: &str,
    context: String,
) -> crate::validation::deferred_types::ProviderTypeCheckSite {
    use crate::validation::deferred_types::ProviderTypeCheckSite;
    let annotation = crate::validation::TypeResolutionContext::Declaration {
        kind,
        name: name.to_string(),
    };
    let location = function
        .type_annotations
        .iter()
        .find(|finding| finding.detail() == &annotation)
        .expect("function annotation parser provenance")
        .location();
    ProviderTypeCheckSite::new(location.clone(), context)
}

fn evaluate_runtime_user_function<B: BindingAddress>(
    function: &crate::parser::UserFunction<B>,
    args: &[crate::resource::FunctionArgument<crate::resource::ParserPlaceholder, B>],
    bindings: &ResolvedBindings<crate::resource::ParserPlaceholder, B>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    call_line: usize,
) -> Result<Value<crate::resource::ParserPlaceholder, B>, crate::parser::ParseError> {
    let required = function
        .params
        .iter()
        .filter(|parameter| parameter.default.is_none())
        .count();
    if args.len() < required || args.len() > function.params.len() {
        return Err(user_function_runtime_error(
            call_line,
            format!(
                "function '{}' expects {}..={} argument(s), got {}",
                function.name,
                required,
                function.params.len(),
                args.len()
            ),
        ));
    }

    let provider_config = config;
    let mut substitutions = HashMap::new();
    for (index, parameter) in function.params.iter().enumerate() {
        let argument = match args.get(index) {
            Some(argument) => argument.clone(),
            None => crate::resource::FunctionArgument::from_value(
                parameter
                    .default
                    .clone()
                    .expect("required function arguments were checked"),
            ),
        };
        let argument = argument
            .try_map_value(|value| {
                resolve_ref_value_inner(value, bindings, RuntimeContext::Session(config))
            })
            .map_err(|message| user_function_runtime_error(call_line, message))?;
        if let Some(expected) = &parameter.param_type {
            let resolved_parameter = crate::validation::resolve_parameter_type_for_check(
                expected,
                provider_config.catalog(),
            )
            .map_err(|message| {
                user_function_runtime_error(
                    call_line,
                    format!(
                        "function '{}': parameter '{}': {message}",
                        function.name, parameter.name
                    ),
                )
            })?;
            if let (Some(resolved_parameter), Some(handle)) =
                (resolved_parameter.as_ref(), argument.handle())
            {
                crate::validation::check_handle_sink(handle, resolved_parameter).map_err(
                    |error| {
                        user_function_runtime_error(
                            call_line,
                            format!(
                                "function '{}': parameter '{}': {error}",
                                function.name, parameter.name
                            ),
                        )
                    },
                )?;
            } else if argument.pending_parameter().is_none()
                && let Some(value) = argument.value()
                && let Some(resolved_constraints) = (value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .ok()
                    .and_then(|value| value.into_known().ok())
            {
                crate::validation::typecheck::TypecheckProviderContext::for_session(
                    provider_config,
                )
                .check_resolved_constraints_at(
                    &resolved_parameter
                        .map_or_else(|| expected.clone(), |resolved| resolved.as_type_expr()),
                    &resolved_constraints,
                    function_annotation_site(
                        function,
                        crate::validation::TypeResolutionDeclarationKind::FunctionParameter,
                        &format!("{}' parameter '{}", function.name, parameter.name),
                        format!(
                            "function '{}' parameter '{}'",
                            function.name, parameter.name
                        ),
                    ),
                )
                .map_err(|error| {
                    user_function_runtime_error(
                        call_line,
                        format!(
                            "function '{}': parameter '{}': {error}",
                            function.name, parameter.name
                        ),
                    )
                })?;
            }
        }
        substitutions.insert(parameter.name.clone(), argument);
    }

    for (name, expression, source_line) in &function.local_lets {
        let substituted =
            crate::parser::substitute_fn_params(expression, &substitutions, *source_line)
                .map_err(|error| locate_user_function_error(function, error))?;
        let resolved =
            resolve_ref_value_inner(&substituted, bindings, RuntimeContext::Session(config))
                .map_err(|message| user_function_runtime_error(*source_line, message))
                .map_err(|error| locate_user_function_error(function, error))?;
        substitutions.insert(
            name.clone(),
            crate::resource::FunctionArgument::from_value(resolved),
        );
    }

    let substituted = crate::parser::substitute_fn_params(
        &function.body.value,
        &substitutions,
        function.body.source_line,
    )
    .map_err(|error| locate_user_function_error(function, error))?;
    let result = resolve_ref_value_inner(&substituted, bindings, RuntimeContext::Session(config))
        .map_err(|message| user_function_runtime_error(function.body.source_line, message))
        .map_err(|error| locate_user_function_error(function, error))?;
    if let Some(expected) = &function.return_type
        && let Some(resolved_constraints) = result
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .ok()
            .and_then(|value| value.into_known().ok())
    {
        crate::validation::typecheck::TypecheckProviderContext::for_session(provider_config)
            .check_resolved_constraints_at(
                expected,
                &resolved_constraints,
                function_annotation_site(
                    function,
                    crate::validation::TypeResolutionDeclarationKind::FunctionReturn,
                    &function.name,
                    format!("function '{}' return value", function.name),
                ),
            )
            .map_err(|error| {
                user_function_runtime_error(
                    function.body.source_line,
                    format!("function '{}': return value: {error}", function.name),
                )
            })
            .map_err(|error| locate_user_function_error(function, error))?;
    }

    Ok(result)
}

pub(crate) fn evaluate_module_resolved_user_function(
    function: &crate::parser::UserFunction<AbsoluteBindingAddress>,
    arguments: &[crate::resource::FunctionArgument<
        crate::resource::ParserPlaceholder,
        AbsoluteBindingAddress,
    >],
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    call_line: usize,
) -> Result<
    Value<crate::resource::ParserPlaceholder, AbsoluteBindingAddress>,
    crate::parser::ParseError,
> {
    evaluate_runtime_user_function(
        function,
        arguments,
        &ResolvedBindings::default(),
        config,
        call_line,
    )
}

fn parser_stage_function_arguments<P: PlaceholderKind, B: BindingAddress>(
    args: &[crate::resource::FunctionArgument<P, B>],
) -> Option<Vec<crate::resource::FunctionArgument<crate::resource::ParserPlaceholder, B>>> {
    let expanded = args
        .iter()
        .cloned()
        .map(crate::resource::FunctionArgument::try_into_expanded)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(
        expanded
            .into_iter()
            .map(crate::resource::FunctionArgument::into_placeholder_type)
            .collect(),
    )
}

fn resolve_function_arguments<P: PlaceholderKind, B: BindingAddress>(
    args: &[crate::resource::FunctionArgument<P, B>],
    bindings: &ResolvedBindings<P, B>,
    config: RuntimeContext<'_>,
) -> Result<Vec<crate::resource::FunctionArgument<P, B>>, ResolutionError> {
    args.iter()
        .map(|argument| {
            argument.try_map_value(|value| resolve_ref_value_inner(value, bindings, config))
        })
        .collect()
}

fn resolve_ref_value_inner<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &ResolvedBindings<P, B>,
    config: RuntimeContext<'_>,
) -> Result<Value<P, B>, ResolutionError> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => Err(
            "unresolved module-template conditional reached runtime resolver"
                .to_string()
                .into(),
        ),
        Value::Deferred(DeferredValue::Coalesce { .. }) => {
            match resolve_ref_value_status_inner(value, bindings, config, false)? {
                ValueResolution::Present(value) | ValueResolution::Unknown(value) => Ok(value),
                // Keep the original expression if a future resolver adds an
                // absent default form. Residual coalesces are rejected at
                // provider/state boundaries, so this cannot silently cross a
                // concrete-value boundary.
                ValueResolution::Absent(_) => Ok(value.clone()),
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            if let Some(attrs) = bindings.get(binding_name)
                && let Some(attr_value) = attrs.get(attribute_name)
            {
                let miss_policy = binding_access_miss_policy(binding_name, bindings);
                return match resolve_historical_selection(
                    attr_value,
                    path,
                    bindings,
                    config,
                    miss_policy,
                ) {
                    Ok(Some(selected)) => Ok(selected),
                    Ok(None) => Ok(value.clone()),
                    Err(HistoricalSelectionError::Access(error)) => {
                        Err(error.format_for_path(&path.to_string()).into())
                    }
                    Err(HistoricalSelectionError::Resolution(error)) => Err(error),
                };
            }
            // Keep as-is if not found
            Ok(value.clone())
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = resolve_ref_value_inner(expression.lhs(), bindings, config)?;
            let rhs = resolve_ref_value_inner(expression.rhs(), bindings, config)?;
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| {
                    let span = expression.operator_span();
                    ResolutionError::from(match (span.line(), span.column()) {
                        (Some(line), Some(column)) => {
                            format!("at line {line}, column {column}: {error}")
                        }
                        _ => error.to_string(),
                    })
                })
        }
        Value::Deferred(DeferredValue::BindingRef { .. }) => Ok(value.clone()),
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value<P, B>>, ResolutionError> = items
                .iter()
                .map(|v| resolve_ref_value_inner(v, bindings, config))
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value<P, B>> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_ref_value_inner(v, bindings, config)?);
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved_parts: Result<Vec<InterpolationPart<P, B>>, ResolutionError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => Ok(InterpolationPart::Expr(
                        resolve_ref_value_inner(v, bindings, config)?,
                    )),
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved_parts?)).canonicalize())
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let resolved_args = resolve_function_arguments(args, bindings, config)?;
            let candidate = Value::Deferred(DeferredValue::FunctionCall {
                name: *name,
                args: resolved_args.clone(),
                source_line: *source_line,
            });
            let knowledge = candidate
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    bindings,
                ))
                .map(|proof| proof.into_classification())
                .map_err(ResolutionError::from)?;
            if !knowledge.permits_static_evaluation() {
                return Ok(candidate);
            }

            // Evaluate the built-in function; propagate errors since args are resolved.
            // The evaluator boundary is `EvalValue::into_value` — a closure
            // returned here would mean `evaluate_builtin_with_config` saw fewer args
            // than the function's arity, which can only happen if a
            // partial application leaked through parsing. The parser is
            // supposed to surface that as a parse error, so treat any
            // closure here as a resolver-level invariant break.
            use crate::eval_value::EvalValue;
            let Some(parser_args) = parser_stage_function_arguments(&resolved_args) else {
                return Ok(Value::Deferred(DeferredValue::FunctionCall {
                    name: *name,
                    args: resolved_args,
                    source_line: *source_line,
                }));
            };
            let eval_args: Vec<EvalValue<B>> = parser_args
                .iter()
                .map(crate::parser::function_argument_as_eval)
                .collect();
            let evaluated = crate::builtins::evaluate_builtin_with_config(
                name,
                &eval_args,
                config.catalog(),
                *source_line,
            );
            match evaluated {
                Ok(result) => result
                    .into_value()
                    .map_err(|error| format!("{name}(): {error}"))?
                    .into_expanded()
                    .map(|value| value.into_placeholder_type())
                    .map_err(|error| ResolutionError::from(format!("{name}(): {error}"))),
                Err(error) => Err(error.with_context(format!("{name}(): "))),
            }
        }
        Value::Deferred(DeferredValue::Unavailable { name, reason, .. }) => {
            Err(format!("{name}(): {reason}").into())
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let session = match config {
                RuntimeContext::Session(session) => session,
                RuntimeContext::Projection(_context) => return Ok(value.clone()),
            };

            // Declaration resolution already owns this failure. Preserve the
            // call just as the parser's pending-function path does.
            if session.function_declaration_failed(function) {
                return Ok(value.clone());
            }

            let resolved_args = resolve_function_arguments(args, bindings, config)?;
            if !resolved_args.iter().all(|argument| {
                argument.handle().is_some()
                    || argument.value().is_some_and(|value| {
                        value
                            .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                                bindings,
                            ))
                            .map(|proof| proof.into_classification())
                            .is_ok_and(|knowledge| knowledge.into_known().is_ok())
                    })
            }) {
                return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                    function: function.clone(),
                    functions: functions.clone(),
                    args: resolved_args,
                    source_line: *source_line,
                }));
            }

            let Some(parser_args) = parser_stage_function_arguments(&resolved_args) else {
                return Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                    function: function.clone(),
                    functions: functions.clone(),
                    args: resolved_args,
                    source_line: *source_line,
                }));
            };
            let evaluated = evaluate_runtime_user_function(
                function,
                &parser_args,
                &ResolvedBindings::default(),
                session,
                source_line.get().unwrap_or(function.body.source_line),
            )
            .map_err(ResolutionError::from)?;
            let evaluated = evaluated
                .into_expanded()
                .map(Value::into_placeholder_type)
                .map_err(|error| error.to_string())?;
            resolve_ref_value_inner(&evaluated, bindings, config)
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let access_expression = crate::value::format_value(value);
            let resolved_base = resolve_ref_value_inner(access.base(), bindings, config)?;
            // Construction canonicalizes reference-rooted access into a
            // ResourceRef/BindingRef. Every surviving Access therefore has a
            // computed container, whose missing element is a final error.
            access
                .rebuild_with_base(resolved_base, AccessMissPolicy::Error)
                .map_err(|error| {
                    let message = error.format_for_path(&access_expression);
                    ResolutionError::from(if access.source_line() == 0 {
                        message
                    } else {
                        format!("at line {}: {message}", access.source_line())
                    })
                })
        }
        Value::Deferred(DeferredValue::Secret(inner)) => inner
            .try_map(|value| resolve_ref_value_inner(value, bindings, config))
            .map(|inner| Value::Deferred(DeferredValue::Secret(inner))),
        // `Value::Unknown` is the result of stamping a previously-
        // unresolved upstream ref; it cannot be resolved further.
        Value::Deferred(DeferredValue::Unknown(_)) => Ok(value.clone()),
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        ) => Ok(value.clone()),
        Value::Deferred(DeferredValue::Placeholder(_)) => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding_index::{BindingCompleteness, SecretPreservingAttributes};
    use crate::resource::ResourceId;
    use std::collections::HashSet;

    type Value = crate::resource::ExpandedValue;
    type ConcreteValue = crate::resource::ExpandedConcreteValue;
    type DeferredValue = crate::resource::ExpandedDeferredValue;

    fn absolute_binding(binding: &str) -> crate::resource::AbsoluteBindingAddress {
        crate::resource::RootScope::new()
            .resolve(crate::resource::RelativeBindingName::new(binding))
    }

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let mut r = Resource::new(
            "test.resource",
            crate::resource::ResourceIdentity::try_from(name.to_string()).unwrap(),
        );
        r.attributes = attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        r.binding = binding.map(absolute_binding);
        r
    }

    /// Build a `ResolvedBindings` from a flat `binding_name → attributes`
    /// map. Each entry is dropped in via `pre_apply` so the resulting
    /// view is identical to what production code constructs — there is
    /// no test-only back door into the type.
    fn bindings_from(entries: Vec<(&str, Vec<(&str, Value)>)>) -> ResolvedBindings {
        let resources: Vec<Resource> = entries
            .into_iter()
            .map(|(binding, attrs)| {
                make_resource(&format!("{}-resource", binding), Some(binding), attrs)
            })
            .collect();
        ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        })
    }

    #[test]
    fn historical_status_requires_the_whole_selected_tree_to_be_known() {
        let bindings = bindings_from(vec![(
            "source",
            vec![(
                "items",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("known".into())),
                    Value::root_resource_ref("future", "identifier", vec![]),
                ])),
            )],
        )]);
        let value = Value::root_resource_ref("source", "items", vec![]);
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                assert!(matches!(
                    resolve_ref_value_status(&value, &bindings, session).unwrap(),
                    ValueResolution::Unknown(_)
                ));
                let element = Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "source",
                        "items",
                        vec![],
                        vec![crate::resource::Subscript::Int { index: 0 }],
                    ),
                });
                assert!(matches!(
                    resolve_ref_value_status(&element, &bindings, session).unwrap(),
                    ValueResolution::Unknown(_)
                ));
            },
        );
    }

    #[test]
    fn historical_access_classifies_its_root_once() {
        let mut nested = Value::Concrete(ConcreteValue::Int(7));
        for _ in 0..20 {
            nested = Value::Concrete(ConcreteValue::List(vec![nested]));
        }
        let bindings = bindings_from(vec![("source", vec![("items", nested)])]);
        let reference = Value::Deferred(DeferredValue::ResourceRef {
            path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                "source",
                "items",
                vec![],
                vec![crate::resource::Subscript::Int { index: 0 }; 20],
            ),
        });
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                crate::resource::knowledge::with_classification_budget(32, || {
                    assert_eq!(
                        resolve_ref_value_status(&reference, &bindings, session).unwrap(),
                        ValueResolution::Present(Value::Concrete(ConcreteValue::Int(7)))
                    );
                });
            },
        );
    }

    fn coalesce(value: Value, default: Value) -> Value {
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(value),
            default: Box::new(default),
        })
    }

    fn integer_expression(
        lhs: Value,
        op: crate::integer_arithmetic::IntegerOp,
        rhs: Value,
    ) -> Value {
        crate::integer_arithmetic::evaluate_integer_values(
            lhs,
            op,
            rhs,
            crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
        )
        .expect("test integer expression should be structurally valid")
    }

    #[test]
    fn deferred_integer_arithmetic_resolves_known_deferred_and_nested_operands() {
        use crate::integer_arithmetic::IntegerOp;

        let expression = integer_expression(
            integer_expression(
                Value::root_resource_ref("source", "seconds", vec![]),
                IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(30)),
            ),
            IntegerOp::Divide,
            Value::root_resource_ref("config", "divisor", vec![]),
        );
        assert_eq!(
            expression.root_bindings(),
            [absolute_binding("config"), absolute_binding("source")].into(),
            "dependency discovery must recurse into both arithmetic operands",
        );
        let bindings = bindings_from(vec![
            (
                "source",
                vec![("seconds", Value::Concrete(ConcreteValue::Int(90)))],
            ),
            (
                "config",
                vec![("divisor", Value::Concrete(ConcreteValue::Int(60)))],
            ),
        ]);

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&expression, &bindings, session)
            ),
            Ok(Value::Concrete(ConcreteValue::Int(2)))
        );

        let partial = bindings_from(vec![(
            "source",
            vec![("seconds", Value::Concrete(ConcreteValue::Int(90)))],
        )]);
        assert!(matches!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&expression, &partial, session)
            ),
            Ok(Value::Deferred(DeferredValue::IntegerArithmetic(_)))
        ));
    }

    #[test]
    fn deferred_integer_arithmetic_reports_runtime_zero_and_overflow() {
        use crate::integer_arithmetic::IntegerOp;

        let divide_by_deferred_zero = integer_expression(
            Value::Concrete(ConcreteValue::Int(100)),
            IntegerOp::Divide,
            Value::root_resource_ref("source", "value", vec![]),
        );
        let zero = bindings_from(vec![(
            "source",
            vec![("value", Value::Concrete(ConcreteValue::Int(0)))],
        )]);
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&divide_by_deferred_zero, &zero, session)
            ),
            Err(ResolutionError::Message(
                "integer division by zero".to_string()
            ))
        );

        let overflow = integer_expression(
            Value::root_resource_ref("source", "value", vec![]),
            IntegerOp::Divide,
            Value::Concrete(ConcreteValue::Int(-1)),
        );
        let minimum = bindings_from(vec![(
            "source",
            vec![("value", Value::Concrete(ConcreteValue::Int(i64::MIN)))],
        )]);
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&overflow, &minimum, session)
            ),
            Err(ResolutionError::Message(
                "integer arithmetic overflow".to_string()
            ))
        );
    }

    #[test]
    fn stamp_unresolved_upstream_propagates_typed_integer_errors() {
        use crate::integer_arithmetic::{
            IntegerArithmeticError, IntegerExpressionError, IntegerOp, IntegerOperatorSpan,
            ValidatedIntegerExpr,
        };

        let expression = ValidatedIntegerExpr::new(
            Value::root_resource_ref("source", "value", vec![]),
            IntegerOp::Divide,
            Value::root_resource_ref("source", "divisor", vec![]),
            IntegerOperatorSpan::synthetic(),
        )
        .expect("resource operands are valid deferred Ints");
        let expression = expression
            .rebuild(
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::Int(1))).into(),
                )),
                Value::Concrete(ConcreteValue::Int(0)),
            )
            .expect("post-validation operand shapes remain valid");

        let error = stamp_unresolved_upstream(
            Value::Deferred(DeferredValue::IntegerArithmetic(expression)),
            &HashSet::new(),
        )
        .expect_err("stamping must not retain a failed arithmetic node");
        assert_eq!(
            error,
            IntegerExpressionError::SecretEvaluation(IntegerArithmeticError::DivideByZero),
        );
    }

    #[test]
    fn user_function_errors_preserve_unavailable_secret_through_parser_context() {
        let snapshot =
            crate::value::redact_secrets_in_value(&Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String(
                    "historical secret".into(),
                )))
                .into(),
            )))
            .unwrap();
        let unavailable = snapshot.to_value_at("state.credentials.token").unwrap_err();
        let located =
            user_function_runtime_error(8, ResolutionError::UnavailableSecret(unavailable.clone()))
                .in_file("/project/functions.crn");
        assert_eq!(
            ResolutionError::from(located),
            ResolutionError::UnavailableSecret(unavailable),
        );

        let ordinary = user_function_runtime_error(8, "ordinary failure".to_string())
            .in_file("/project/functions.crn");
        assert_eq!(
            ResolutionError::from(ordinary).to_string(),
            "/project/functions.crn: Invalid expression at line 8: ordinary failure",
        );
    }

    #[test]
    fn element_owned_function_errors_preserve_unavailable_secret() {
        let parsed = crate::config_loader::parse_directory_files(
            &[
                ("/project/main.crn".into(), "let unrelated = 'main'".into()),
                (
                    "/project/functions.crn".into(),
                    "fn identity() { 'value' }".into(),
                ),
            ],
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap()
        .remove(1)
        .1
        .into_inner();
        let snapshot =
            crate::value::redact_secrets_in_value(&Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String(
                    "historical secret".into(),
                )))
                .into(),
            )))
            .unwrap();
        let unavailable = snapshot.to_value_at("state.credentials.token").unwrap_err();
        let error = crate::parser::ParseError::AtElement(Box::new(crate::parser::Located::new(
            parsed.user_functions["identity"].body.location.clone(),
            crate::parser::ParseError::UnavailableSecret(unavailable.clone()),
        )));
        assert_eq!(
            ResolutionError::from(error),
            ResolutionError::UnavailableSecret(unavailable)
        );
    }

    #[test]
    fn coalesce_resolution_is_present_absent_or_unknown() {
        let default = Value::Concrete(ConcreteValue::List(Vec::new()));
        let accounts = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("a".into()),
        )]));
        let expression = coalesce(
            Value::root_resource_ref("up", "accounts", Vec::new()),
            default.clone(),
        );

        let mut present = ResolvedBindings::default();
        present.set(
            "up",
            HashMap::from([("accounts".to_string(), accounts.clone())]),
            BindingValueSource::Upstream,
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&expression, &present, session)
            ),
            Ok(ValueResolution::Present(accounts)),
        );

        let mut absent = ResolvedBindings::default();
        absent.set("up", HashMap::new(), BindingValueSource::Upstream);
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&expression, &absent, session)
            ),
            Ok(ValueResolution::Present(default)),
        );

        let mut unknown = ResolvedBindings::default();
        unknown.set_with_completeness(
            "up",
            HashMap::new(),
            BindingValueSource::Upstream,
            BindingCompleteness::Incomplete,
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&expression, &unknown, session)
            ),
            Ok(ValueResolution::Unknown(expression)),
        );
    }

    #[test]
    fn coalesce_groupings_propagate_absence_associatively() {
        let first = Value::root_resource_ref("r", "comment", Vec::new());
        let second = Value::root_resource_ref("r", "parent", Vec::new());
        let fallback = Value::Concrete(ConcreteValue::String("default".into()));
        let left_grouped = coalesce(coalesce(first.clone(), second.clone()), fallback.clone());
        let right_grouped = coalesce(first, coalesce(second, fallback.clone()));
        let cases = [
            (
                HashMap::from([
                    (
                        "comment".to_string(),
                        Value::Concrete(ConcreteValue::String("first".into())),
                    ),
                    (
                        "parent".to_string(),
                        Value::Concrete(ConcreteValue::String("second".into())),
                    ),
                ]),
                Value::Concrete(ConcreteValue::String("first".into())),
            ),
            (
                HashMap::from([(
                    "parent".to_string(),
                    Value::Concrete(ConcreteValue::String("second".into())),
                )]),
                Value::Concrete(ConcreteValue::String("second".into())),
            ),
            (HashMap::new(), fallback.clone()),
        ];

        for (attributes, expected) in cases {
            let mut bindings = ResolvedBindings::default();
            bindings.set_with_completeness(
                "r",
                attributes,
                BindingValueSource::Local,
                BindingCompleteness::Complete,
            );
            for expression in [&left_grouped, &right_grouped] {
                assert_eq!(
                    crate::validation::deferred_types::with_session_for_test(
                        &crate::parser::LoadedProviderContext::empty_for_test(),
                        |session| resolve_ref_value_status(expression, &bindings, session)
                    ),
                    Ok(ValueResolution::Present(expected.clone())),
                );
            }
        }
    }

    #[test]
    fn pending_secret_attribute_keeps_coalesce_undecided() {
        let pending = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::PendingReRead(
                    crate::resource::PendingReRead::not_found(),
                ),
            )))
            .into(),
        ));
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "holder",
            HashMap::from([("comment".into(), pending)]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );
        let expression = coalesce(
            Value::root_resource_ref("holder", "comment", vec![]),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        let result = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value_status(&expression, &bindings, session),
        )
        .unwrap();
        assert!(matches!(
            result,
            ValueResolution::Unknown(Value::Deferred(DeferredValue::Coalesce { .. }))
        ));
    }

    #[test]
    fn coalesce_does_not_treat_explicit_unknown_as_absent() {
        let unknown_reason = crate::resource::ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::incomplete("provider read incomplete"),
        );
        let expression = coalesce(
            Value::Deferred(DeferredValue::Unknown(unknown_reason)),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value_status(&expression, &ResolvedBindings::default(), session),
        )
        .expect("unknown coalesce must remain resolvable");
        assert!(matches!(
            resolved,
            ValueResolution::Unknown(Value::Deferred(DeferredValue::Coalesce { .. }))
        ));
    }

    #[test]
    fn coalesce_does_not_evaluate_function_with_unknown_argument() {
        let function = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![
                Value::Deferred(DeferredValue::Unknown(
                    crate::resource::ApplyTimeUnknown::PendingReRead(
                        crate::resource::PendingReRead::incomplete("producer still running"),
                    ),
                ))
                .into(),
            ],
            source_line: 1.into(),
        });
        let expression = coalesce(
            function,
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        assert!(matches!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(
                    &expression,
                    &ResolvedBindings::default(),
                    session
                )
            ),
            Ok(ValueResolution::Unknown(Value::Deferred(
                DeferredValue::Coalesce { .. }
            )))
        ));
    }

    #[test]
    fn coalesce_does_not_treat_function_error_as_absence() {
        let expression = coalesce(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: vec![Value::Concrete(ConcreteValue::Int(1)).into()],
                source_line: 1.into(),
            }),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        let error = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value_status(&expression, &ResolvedBindings::default(), session),
        )
        .expect_err("function errors must not select the coalesce default");
        assert!(
            error.to_string().contains("upper()"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn selected_coalesce_fallback_propagates_unavailable_builtin_error() {
        let missing = "CARINA_TEST_ISSUE_3974_SELECTED_FALLBACK_MISSING";
        let expression = coalesce(
            Value::root_resource_ref("r", "comment", Vec::new()),
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: vec![Value::Concrete(ConcreteValue::String(missing.into())).into()],
                reason: crate::resource::UnavailableReason::from_builtin_error(format!(
                    "environment variable '{missing}' is not set"
                )),
                source_line: 1.into(),
            }),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "r",
            HashMap::new(),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&expression, &bindings, session)
            ),
            Err(ResolutionError::Message(format!(
                "env(): environment variable '{missing}' is not set"
            ))),
        );
    }

    #[test]
    fn incomplete_binding_distinguishes_desired_values_from_provisional_outputs() {
        let resource = make_resource(
            "up-resource",
            Some("up"),
            vec![
                ("name", Value::Concrete(ConcreteValue::String("new".into()))),
                (
                    "stable",
                    Value::Concrete(ConcreteValue::String("same".into())),
                ),
            ],
        );
        let state = State::existing(
            resource.id.clone(),
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
                (
                    "accounts".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String("old-account".into()),
                    )])),
                ),
                (
                    "stable".to_string(),
                    Value::Concrete(ConcreteValue::String("same".into())),
                ),
                (
                    "removed".to_string(),
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
            ]),
        )
        .with_identifier("up-id");
        let current = crate::resource::into_plan_input_current_map_from_states(
            HashMap::from([(resource.id.clone(), state)]),
            &crate::schema::SchemaRegistry::new(),
            std::slice::from_ref(&resource),
        );
        let stability = HashMap::from([(
            resource.id.clone(),
            crate::binding_index::ManagedBindingStability::Updating {
                changed_attributes: HashSet::from(["name".to_string(), "removed".to_string()]),
                refreshable_attributes: HashSet::from([
                    "accounts".to_string(),
                    "missing_output".to_string(),
                ]),
            },
        )]);
        let bindings = ResolvedBindings::pre_apply_live_with_stability(
            crate::binding_index::LivePreApplyInputs {
                managed: std::slice::from_ref(&resource),
                compositions: &[],
                managed_current: &current,
                data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            },
            &stability,
        );

        let changed = coalesce(
            Value::root_resource_ref("up", "name", vec![]),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&changed, &bindings, session)
            ),
            Ok(ValueResolution::Present(Value::Concrete(
                ConcreteValue::String("new".into()),
            ))),
            "a resolved authored desired value overrides refreshed state during its update",
        );

        let removed_default = Value::Concrete(ConcreteValue::String("removed-default".into()));
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(
                    &coalesce(
                        Value::root_resource_ref("up", "removed", vec![]),
                        removed_default.clone(),
                    ),
                    &bindings,
                    session
                )
            ),
            Ok(ValueResolution::Present(removed_default)),
            "an attribute removed by the update is definitively absent",
        );

        let provisional = coalesce(
            Value::root_resource_ref("up", "accounts", vec![]),
            Value::Concrete(ConcreteValue::List(Vec::new())),
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&provisional, &bindings, session)
            ),
            Ok(ValueResolution::Present(Value::Concrete(
                ConcreteValue::List(vec![Value::Concrete(ConcreteValue::String(
                    "old-account".into(),
                ))]),
            ))),
            "a present provider-populated value remains usable during an unrelated update",
        );

        let missing_refreshable = coalesce(
            Value::root_resource_ref("up", "missing_output", vec![]),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&missing_refreshable, &bindings, session)
            ),
            Ok(ValueResolution::Unknown(missing_refreshable)),
            "a missing provider-populated value may still appear during refresh",
        );

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(
                    &coalesce(
                        Value::root_resource_ref("up", "stable", vec![]),
                        Value::Concrete(ConcreteValue::String("fallback".into())),
                    ),
                    &bindings,
                    session
                )
            ),
            Ok(ValueResolution::Present(Value::Concrete(
                ConcreteValue::String("same".into()),
            ))),
            "an unchanged attribute remains definitive during an update",
        );
        let missing_default = Value::Concrete(ConcreteValue::String("fallback".into()));
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(
                    &coalesce(
                        Value::root_resource_ref("up", "unrelated_optional", vec![]),
                        missing_default.clone(),
                    ),
                    &bindings,
                    session
                )
            ),
            Ok(ValueResolution::Present(missing_default)),
            "absence of an attribute the update cannot change remains definitive",
        );
    }

    #[test]
    fn unchanged_ref_authored_producer_has_complete_refreshed_outputs() {
        let source = make_resource(
            "source",
            Some("source"),
            vec![(
                "name",
                Value::Concrete(ConcreteValue::String("seed".into())),
            )],
        );
        let producer = make_resource(
            "producer",
            Some("producer"),
            vec![(
                "parent_name",
                Value::root_resource_ref("source", "name", vec![]),
            )],
        );
        let source_state = State::existing(
            source.id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("seed".into())),
            )]),
        )
        .with_identifier("source-id");
        let producer_state = State::existing(
            producer.id.clone(),
            HashMap::from([(
                "parent_name".to_string(),
                Value::Concrete(ConcreteValue::String("seed".into())),
            )]),
        )
        .with_identifier("producer-id");
        let resources = [source, producer];
        let current = crate::resource::into_plan_input_current_map_from_states(
            HashMap::from([
                (resources[0].id.clone(), source_state),
                (resources[1].id.clone(), producer_state),
            ]),
            &crate::schema::SchemaRegistry::new(),
            &resources,
        );
        let bindings =
            ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs::<
                crate::resource::UnexpandedDependencyFields,
            > {
                managed: &resources,
                compositions: &[],
                managed_current: &current,
                data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            });
        let fallback = Value::Concrete(ConcreteValue::String("d".into()));
        let expression = coalesce(
            Value::root_resource_ref("producer", "comment", vec![]),
            fallback.clone(),
        );

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&expression, &bindings, session)
            ),
            Ok(ValueResolution::Present(fallback)),
            "a successful refresh with no planned effect makes a missing output definitively absent",
        );
    }

    #[test]
    fn post_apply_complete_binding_treats_missing_output_as_absent() {
        let resource = make_resource("producer", Some("producer"), vec![]);
        let state: crate::resource::State =
            State::existing(resource.id.clone(), HashMap::new()).with_identifier("provider-id");
        let mut bindings = ResolvedBindings::default();
        let empty: HashMap<String, Value> = HashMap::new();
        let resolved = SecretPreservingAttributes::from_provider_values(&empty, &empty);
        bindings.record_applied(
            Some("producer"),
            &resolved,
            &state,
            crate::binding_index::BindingCompleteness::Complete,
        );
        let default = Value::Concrete(ConcreteValue::String("fallback".into()));
        let expression = coalesce(
            Value::root_resource_ref("producer", "optional", vec![]),
            default.clone(),
        );

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value_status(&expression, &bindings, session)
            ),
            Ok(ValueResolution::Present(default)),
        );
    }

    #[test]
    fn coalesce_propagates_wrong_shape_errors() {
        use crate::resource::{AccessPath, PathSegment};

        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "up",
            HashMap::from([(
                "accounts".to_string(),
                Value::Concrete(ConcreteValue::String("not-a-map".into())),
            )]),
            BindingValueSource::Upstream,
        );
        let expression = coalesce(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root_with_segments(
                    "up",
                    "accounts",
                    vec![PathSegment::Field {
                        name: "missing".to_string(),
                    }],
                ),
            }),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );

        let error = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value_status(&expression, &bindings, session),
        )
        .expect_err("wrong-shaped access must remain an error");
        assert!(
            error.to_string().contains("cannot apply access"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn coalesce_does_not_treat_missing_field_on_computed_value_as_optional() {
        use crate::resource::{AccessMissPolicy, PathSegment, apply_postfix_access};

        let mut choices = IndexMap::new();
        choices.insert(
            "selected".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "present".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )]))),
        );
        let computed = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::lookup,
            args: vec![
                Value::Concrete(ConcreteValue::Map(choices)).into(),
                Value::root_resource_ref("selector", "key", vec![]).into(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())).into(),
            ],
            source_line: 1.into(),
        });
        let missing = apply_postfix_access(
            computed,
            PathSegment::Field {
                name: "missing".to_string(),
            },
            1,
            AccessMissPolicy::Error,
        )
        .expect("deferred computed access");
        let expression = coalesce(
            missing,
            Value::Concrete(ConcreteValue::String("fallback".into())),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "selector",
            HashMap::from([(
                "key".to_string(),
                Value::Concrete(ConcreteValue::String("selected".into())),
            )]),
            BindingValueSource::Upstream,
        );

        let error = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value_status(&expression, &bindings, session),
        )
        .expect_err("only an access chain rooted at a reference is optional");
        assert!(
            error.to_string().contains("missing"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn local_bare_binding_ref_stays_typed_during_general_resolution() {
        let bindings = bindings_from(vec![("vpc", vec![])]);
        let reference = Value::Deferred(DeferredValue::BindingRef {
            binding: absolute_binding("vpc"),
        });

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&reference, &bindings, session)
            ),
            Ok(reference)
        );
    }

    /// Test-only adapter that preserves the pre-#3248 four-arg
    /// signature so the bulky test corpus migrates mechanically.
    /// Builds the bindings view here, then delegates to the new
    /// signature.
    fn resolve_refs_with_state_and_remote_legacy(
        resources: &mut [Resource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[crate::binding_index::WaitAliasSpec],
    ) -> Result<(), ResolutionError> {
        let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: resources,
            compositions: &[],
            managed_current: &plan_input_states,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings,
            wait_aliases,
        });
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_refs_with_state_and_remote(resources, &bindings, session),
        )
    }

    /// Test-only adapter for the plan-path resolver (mirrors
    /// `resolve_refs_with_state_and_remote_legacy`).
    fn resolve_refs_for_plan_legacy(
        resources: &mut [Resource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[crate::binding_index::WaitAliasSpec],
    ) -> Result<(), ResolutionError> {
        let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: resources,
            compositions: &[],
            managed_current: &plan_input_states,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings,
            wait_aliases,
        });
        let upstream_keys: std::collections::HashSet<&str> =
            remote_bindings.keys().map(String::as_str).collect();
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_refs_for_plan(resources, &bindings, &upstream_keys, session),
        )
    }

    #[test]
    fn test_resolve_subscript_descends_into_list() {
        // `orgs.accounts[0]` against `accounts: [a, b]` resolves to `a`.
        let bindings = bindings_from(vec![(
            "orgs",
            vec![(
                "accounts",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("alpha".into())),
                    Value::Concrete(ConcreteValue::String("beta".into())),
                ])),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 0 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("alpha".into()))
        );
    }

    #[test]
    fn test_resolve_chained_subscript_then_field_descends_into_struct() {
        // carina#3025 reproduction: `cert.list[0].name` against
        // `list: [ { name = "alpha" }, { name = "beta" } ]` resolves
        // to `"alpha"`. The resolver walks segments in source order
        // (Subscript then Field), so the post-subscript field access
        // descends into the inner map.
        use crate::resource::{AccessPath, PathSegment, Subscript};
        let inner_alpha: indexmap::IndexMap<String, Value> = vec![(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("alpha".into())),
        )]
        .into_iter()
        .collect();
        let inner_beta: indexmap::IndexMap<String, Value> = vec![(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("beta".into())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "cert",
            vec![(
                "list",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Map(inner_alpha)),
                    Value::Concrete(ConcreteValue::Map(inner_beta)),
                ])),
            )],
        )]);
        let path = AccessPath::root_with_segments(
            "cert",
            "list",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "name".to_string(),
                },
            ],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("alpha".into()))
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_map() {
        // `orgs.accounts["alpha"]` against `accounts: { alpha = "1", beta = "2" }`
        // resolves to `"1"`.
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "alpha".to_string(),
                Value::Concrete(ConcreteValue::String("1".into())),
            ),
            (
                "beta".to_string(),
                Value::Concrete(ConcreteValue::String("2".into())),
            ),
        ]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "orgs",
            vec![("accounts", Value::Concrete(ConcreteValue::Map(map)))],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "alpha".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, Value::Concrete(ConcreteValue::String("1".into())));
    }

    #[test]
    fn test_resolve_subscript_descends_into_secret_wrapped_map() {
        // #2439: subscript on `Secret(Map)` must descend and re-wrap
        // so plan-display redaction survives end-to-end.
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Concrete(ConcreteValue::String("hunter2".into())),
            ),
            (
                "api_key".to_string(),
                Value::Concrete(ConcreteValue::String("xyz".into())),
            ),
        ]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "creds_binding",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::Map(map))).into(),
                )),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "creds_binding",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "db_pwd".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String("hunter2".into()))).into()
            )),
            "subscript on Secret(Map) must project the entry and re-wrap as Secret(String)"
        );
    }

    #[test]
    fn test_resolve_subscript_descends_into_secret_wrapped_list() {
        // Symmetric with the map test: `Value::Secret(Box::new(
        // Value::Concrete(ConcreteValue::List(...))))` + integer subscript projects the element
        // and re-wraps the leaf as `Value::Secret`.
        let bindings = bindings_from(vec![(
            "secret_holder",
            vec![(
                "tokens",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::String("alpha".into())),
                        Value::Concrete(ConcreteValue::String("beta".into())),
                    ])))
                    .into(),
                )),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "secret_holder",
            "tokens",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 1 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String("beta".into()))).into()
            )),
        );
    }

    #[test]
    fn test_resolve_field_path_descends_into_secret_wrapped_map() {
        // #2439 sibling: dot-form field access on `Secret(Map)` walks
        // the same blind spot as the subscript path. Pin the symmetric
        // fix so a regression that drops the field_path peel doesn't
        // silently keep the ref.
        let map: indexmap::IndexMap<String, Value> = vec![(
            "db_pwd".to_string(),
            Value::Concrete(ConcreteValue::String("hunter2".into())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "creds_binding",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::Map(map))).into(),
                )),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields(
            "creds_binding",
            "creds",
            vec!["db_pwd".to_string()],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String("hunter2".into()))).into()
            )),
        );
    }

    #[test]
    fn review2_f2_resolving_secret_reference_matches_main_single_wrapper() {
        let secret = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String("t".into()))).into(),
        ));
        let bindings = bindings_from(vec![("a", vec![("token", secret.clone())])]);
        let value = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::root_resource_ref("a", "token", vec![])).into(),
        ));
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, secret);
    }

    #[test]
    fn test_resolve_subscript_descends_into_doubly_secret_wrapped_map() {
        // Main's defensive stacked-wrapper fixture tested peel/rewrap depth.
        // SecretPayload now collapses those layers. Navigation, hash bytes,
        // provider plaintext and display retain the same observable value.
        let map: indexmap::IndexMap<String, Value> = vec![(
            "k".to_string(),
            Value::Concrete(ConcreteValue::String("v".into())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "b",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Deferred(DeferredValue::Secret(
                        Box::new(Value::Concrete(ConcreteValue::Map(map))).into(),
                    )))
                    .into(),
                )),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "b",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str { key: "k".into() }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String("v".into()))).into()
            )),
        );
        assert_eq!(crate::value::format_value(&resolved), "(secret)");
        assert_eq!(
            crate::value::canonical_secret_hash_material(&resolved).unwrap(),
            br#""v""#
        );
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_list_out_of_range_keeps_ref() {
        // Parity with `test_resolve_subscript_out_of_range_keeps_ref`
        // for the `Secret(List)` shape: out-of-range index keeps the
        // original ref unchanged (the planner surfaces it as
        // unresolved). Pin so a future refactor of the rewrap path
        // can't accidentally wrap the kept ref in `Secret`.
        let bindings = bindings_from(vec![(
            "h",
            vec![(
                "tokens",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String("only".into()),
                    )])))
                    .into(),
                )),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "h",
            "tokens",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_map_missing_key_local_keeps_ref() {
        // Local binding (DSL-side `let`) with a `Secret(Map)` value:
        // a missing key keeps the ref unchanged because the value may
        // not be resolved yet. Mirrors the non-secret local-missing
        // path, just with the Secret peel.
        let map: indexmap::IndexMap<String, Value> = vec![(
            "known".to_string(),
            Value::Concrete(ConcreteValue::String("v".into())),
        )]
        .into_iter()
        .collect();
        let bindings = bindings_from(vec![(
            "h",
            vec![(
                "creds",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::Map(map))).into(),
                )),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "missing".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_map_missing_key_upstream_errors() {
        // Upstream binding with a `Secret(Map)` value: a missing key
        // surfaces `missing_map_key_error` (concrete state, so a
        // missing key is a real typo). Pin the error path through the
        // peel so a refactor can't accidentally swallow it.
        //
        // #2501: the available-keys list must be redacted (entry count
        // only, no key names) because the keys of a Secret(Map) leak
        // shape — e.g. "this binding exports a `db_pwd`, an `api_key`".
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String("v".into()))).into(),
                )),
            ),
            (
                "api_key".to_string(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String("v".into()))).into(),
                )),
            ),
            (
                "slack_token".to_string(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String("v".into()))).into(),
                )),
            ),
        ]
        .into_iter()
        .collect();
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::Map(map))).into(),
            )),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("h", attrs, BindingValueSource::Upstream);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "missing".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let result = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        );
        let err = result.expect_err("missing key in concrete upstream Secret(Map) must error");
        assert_eq!(
            err.to_string(),
            "h.creds['missing']: key not found; available keys: <redacted, 3 entries>"
        );
        for k in ["db_pwd", "api_key", "slack_token"] {
            assert!(
                !err.to_string().contains(k),
                "Secret(Map) key {k:?} leaked into error: {err}"
            );
        }
    }

    #[test]
    fn test_resolve_field_path_secret_wrapped_map_missing_key_upstream_redacted() {
        // #2501 sibling: dot-form field access (`creds.missing`) on a
        // `Secret(Map)` upstream binding must also redact the available
        // keys. The field_path walk and the subscript walk share the
        // same `missing_map_key_error_redacted` helper, so both must
        // emit the redacted message.
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let map: indexmap::IndexMap<String, Value> = vec![
            (
                "db_pwd".to_string(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String("v".into()))).into(),
                )),
            ),
            (
                "api_key".to_string(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String("v".into()))).into(),
                )),
            ),
        ]
        .into_iter()
        .collect();
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::Map(map))).into(),
            )),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("h", attrs, BindingValueSource::Upstream);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "h",
            "creds",
            vec!["missing".to_string()],
            Vec::new(),
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let err = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .expect_err("missing dot-form field in concrete upstream Secret(Map) must error");
        assert_eq!(
            err.to_string(),
            "h.creds.missing: key not found; available keys: <redacted, 2 entries>"
        );
        for k in ["db_pwd", "api_key"] {
            assert!(
                !err.to_string().contains(k),
                "Secret(Map) key {k:?} leaked into field_path error: {err}"
            );
        }
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_empty_map_missing_key_upstream_redacted() {
        // Edge case: an empty `Secret(Map)` still emits the redacted
        // form (entry count = 0). Pins that the redacted helper does
        // not fall back to the literal "<no keys>" branch, which would
        // also be safe for an empty map but inconsistent with the
        // populated-Secret message format.
        use crate::binding_index::{BindingValueSource, ResolvedBindings};
        let mut attrs = HashMap::new();
        attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::Map(
                    indexmap::IndexMap::new(),
                )))
                .into(),
            )),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("h", attrs, BindingValueSource::Upstream);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str {
                key: "missing".into(),
            }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let err = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .expect_err("missing key in empty upstream Secret(Map) must error");
        assert_eq!(
            err.to_string(),
            "h.creds['missing']: key not found; available keys: <redacted, 0 entries>"
        );
    }

    #[test]
    fn test_resolve_subscript_secret_wrapped_unresolved_inner_ref_stays_deferred() {
        // Defensive: after the initial `resolve_ref_value(attr_value)`
        // call, the `resolved` value can be a `Secret(ResourceRef)` if
        // the inner ref points to an as-yet-unresolved binding. The
        // peel/rewrap loops then expose the inner ResourceRef and the
        // shared access primitive must preserve the Secret wrapper and
        // retain the pending projection as Access — never strip the
        // Secret tag or pretend the projection is concrete.
        let mut h_attrs: HashMap<String, Value> = HashMap::new();
        h_attrs.insert(
            "creds".to_string(),
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::root_resource_ref(
                    "missing".to_string(),
                    "x".to_string(),
                    vec![],
                ))
                .into(),
            )),
        );
        let mut bindings = crate::binding_index::ResolvedBindings::default();
        bindings.set(
            "h",
            h_attrs,
            crate::binding_index::BindingValueSource::Local,
        );
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "h",
            "creds",
            Vec::new(),
            vec![crate::resource::Subscript::Str { key: "k".into() }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved, ref_value,
            "Secret(ResourceRef) projection must keep the outer ref unchanged"
        );
    }

    #[test]
    fn test_resolve_unresolved_secret_projection_with_hashed_sibling_stays_deferred() {
        use crate::resource::SnapshotValue;

        let hash = crate::value::redact_secrets_in_value(&Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(
                "unrelated secret".into(),
            )))
            .into(),
        )))
        .unwrap();
        let pending = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::root_resource_ref("missing", "x", vec![])).into(),
        ));
        let historical = SnapshotValue::Concrete(crate::resource::ConcreteValue::Map(
            indexmap::IndexMap::from([
                ("hash".into(), hash),
                ("pending".into(), pending.to_snapshot()),
            ]),
        ));
        let mut bindings = ResolvedBindings::default();
        bindings.set_historical_with_completeness(
            "h",
            HashMap::from([("creds".into(), historical)]),
            crate::binding_index::BindingValueSource::Local,
            BindingCompleteness::Incomplete,
        );
        let reference = Value::Deferred(DeferredValue::ResourceRef {
            path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                "h",
                "creds",
                vec!["pending".into()],
                vec![crate::resource::Subscript::Str { key: "k".into() }],
            ),
        });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&reference, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, reference);
    }

    #[test]
    fn test_resolve_subscript_out_of_range_keeps_ref() {
        // `orgs.accounts[5]` against a one-element local list keeps the
        // reference: a local desired-side view may still be incomplete.
        let bindings = bindings_from(vec![(
            "orgs",
            vec![(
                "accounts",
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("a".into()),
                )])),
            )],
        )]);
        let path = crate::resource::AccessPath::root_with_fields_and_subscripts(
            "orgs",
            "accounts",
            Vec::new(),
            vec![crate::resource::Subscript::Int { index: 5 }],
        );
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_simple_resource_ref() {
        let bindings = bindings_from(vec![(
            "my_vpc",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            )],
        )]);

        let ref_value = Value::root_resource_ref("my_vpc".to_string(), "id".to_string(), vec![]);

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-123".into()))
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_list() {
        let bindings = bindings_from(vec![(
            "my_sg",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("sg-456".into())),
            )],
        )]);

        let list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("static".into())),
            Value::root_resource_ref("my_sg".to_string(), "id".to_string(), vec![]),
        ]));

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&list, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("static".into())),
                Value::Concrete(ConcreteValue::String("sg-456".into())),
            ]))
        );
    }

    #[test]
    fn test_resolve_nested_refs_in_map() {
        let bindings = bindings_from(vec![(
            "my_subnet",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("subnet-789".into())),
            )],
        )]);

        let map = Value::Concrete(ConcreteValue::Map(
            vec![(
                "subnet_id".to_string(),
                Value::root_resource_ref("my_subnet".to_string(), "id".to_string(), vec![]),
            )]
            .into_iter()
            .collect(),
        ));

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&map, &bindings, session),
        )
        .unwrap();
        if let Value::Concrete(ConcreteValue::Map(m)) = resolved {
            assert_eq!(
                m.get("subnet_id"),
                Some(&Value::Concrete(ConcreteValue::String("subnet-789".into())))
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_unresolved_ref_stays_as_is() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let ref_value =
            Value::root_resource_ref("nonexistent".to_string(), "id".to_string(), vec![]);

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn test_resolve_interpolation_all_resolved() {
        let bindings = bindings_from(vec![(
            "my_vpc",
            vec![(
                "vpc_id",
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            )],
        )]);

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("subnet-".into()),
            InterpolationPart::Expr(Value::root_resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]));

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&interp, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("subnet-vpc-123".into()))
        );
    }

    #[test]
    fn test_resolve_interpolation_partially_unresolved() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("subnet-".into()),
            InterpolationPart::Expr(Value::root_resource_ref(
                "my_vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ]));

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&interp, &bindings, session),
        )
        .unwrap();
        // Should remain as Interpolation since the ref couldn't be resolved
        assert!(matches!(
            resolved,
            Value::Deferred(DeferredValue::Interpolation(_))
        ));
    }

    #[test]
    fn test_resolve_interpolation_with_non_string_types() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let interp = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("port-".into()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::Int(8080))),
            InterpolationPart::Literal("-enabled-".into()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::Bool(true))),
        ]));

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&interp, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("port-8080-enabled-true".into()))
        );
    }

    #[test]
    fn test_state_attributes_merged_into_binding_map() {
        let rid = ResourceId::with_identity("test.resource", "my-vpc");
        let mut resources = vec![
            make_resource(
                "my-vpc",
                Some("my_vpc"),
                vec![(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                )],
            ),
            make_resource(
                "my-subnet",
                None,
                vec![(
                    "vpc_id",
                    Value::root_resource_ref("my_vpc".to_string(), "vpc_id".to_string(), vec![]),
                )],
            ),
        ];

        let mut current_states = HashMap::new();
        current_states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: true,
                attributes: vec![(
                    "vpc_id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-abc".into())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: Default::default(),

                partial_read: None,
            },
        );

        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                resolve_refs_with_state(
                    &mut resources,
                    &crate::resource::into_plan_input_current_map_from_states(
                        current_states.clone(),
                        &crate::schema::SchemaRegistry::new(),
                        &[],
                    ),
                    session,
                )
            },
        )
        .unwrap();

        // The subnet's vpc_id should be resolved from state
        assert_eq!(
            resources[1].get_attr("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String("vpc-abc".into())))
        );
    }

    #[test]
    fn test_resolve_function_call_join() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("a".into())),
                    Value::Concrete(ConcreteValue::String("b".into())),
                    Value::Concrete(ConcreteValue::String("c".into())),
                ]))
                .into(),
            ],
            source_line: (0).into(),
        });

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&func, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("a-b-c".into()))
        );
    }

    #[test]
    fn test_resolve_function_call_with_resource_ref() {
        let bindings = bindings_from(vec![(
            "vpc",
            vec![(
                "id",
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            )],
        )]);

        // join("-", ["prefix", vpc.id]) should resolve vpc.id first, then evaluate
        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("prefix".into())),
                    Value::root_resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                ]))
                .into(),
            ],
            source_line: (0).into(),
        });

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&func, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("prefix-vpc-123".into()))
        );
    }

    #[test]
    fn access_over_deferred_function_base_reduces_after_reference_resolution() {
        use crate::resource::{PathSegment, Subscript};

        let bindings = bindings_from(vec![(
            "queue",
            vec![(
                "arn",
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:sqs:region:account:name".into(),
                )),
            )],
        )]);
        let value = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String(":".into())).into(),
                    Value::root_resource_ref("queue", "arn", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&value, &bindings, session)
            )
            .unwrap(),
            Value::Concrete(ConcreteValue::String("name".into()))
        );
    }

    #[test]
    fn access_over_deferred_function_base_reports_out_of_range_after_resolution() {
        use crate::binding_index::BindingValueSource;
        use crate::resource::{PathSegment, Subscript};

        let mut attrs = HashMap::new();
        attrs.insert(
            "arn".to_string(),
            Value::Concrete(ConcreteValue::String("a:b:c".into())),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("queue", attrs, BindingValueSource::Upstream);
        let value = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String(":".into())).into(),
                    Value::root_resource_ref("queue", "arn", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        let error = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&value, &bindings, session),
        )
        .expect_err("apply-time out-of-range access must be an error");
        assert!(
            error
                .to_string()
                .contains("index 5 out of range for list of length 3"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn computed_access_over_local_reference_reports_out_of_range_immediately() {
        use crate::binding_index::BindingValueSource;
        use crate::resource::{PathSegment, Subscript};

        let mut attrs = HashMap::new();
        attrs.insert(
            "identifier".to_string(),
            Value::Concrete(ConcreteValue::String("a-b-c".into())),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("q", attrs, BindingValueSource::Local);
        let value = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("-".into())).into(),
                    Value::root_resource_ref("q", "identifier", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            7,
            AccessMissPolicy::Error,
        )
        .unwrap();

        let error = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&value, &bindings, session),
        )
        .expect_err("a computed list is final even when an input ref is local");
        assert!(
            error
                .to_string()
                .contains("index 5 out of range for list of length 3"),
            "unexpected error: {error}"
        );
        assert!(
            !error.to_string().contains("wait"),
            "range errors must not suggest wait: {error}"
        );
    }

    #[test]
    fn local_resource_path_and_postfix_access_both_defer_missing_map_key() {
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let mut tags = IndexMap::new();
        tags.insert(
            "present".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        );
        let tags_value = Value::Concrete(ConcreteValue::Map(tags));
        let bindings = bindings_from(vec![("res", vec![("tags", tags_value.clone())])]);
        let step = PathSegment::Subscript {
            index: Subscript::Str {
                key: "missing".into(),
            },
        };
        let resource_path = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments("res", "tags", vec![step.clone()]),
        });
        let postfix_access = crate::resource::apply_postfix_access(
            Value::root_resource_ref("res".to_string(), "tags".to_string(), vec![]),
            step,
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(postfix_access, resource_path);

        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&resource_path, &bindings, session)
            ),
            Ok(resource_path.clone())
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&postfix_access, &bindings, session)
            ),
            Ok(resource_path.clone())
        );
    }

    #[test]
    fn upstream_resource_path_and_postfix_access_both_error_on_missing_map_key() {
        use crate::binding_index::BindingValueSource;
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let mut tags = IndexMap::new();
        tags.insert(
            "present".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(tags)),
        );
        let mut bindings = ResolvedBindings::default();
        bindings.set("res", attrs, BindingValueSource::Upstream);
        let step = PathSegment::Subscript {
            index: Subscript::Str {
                key: "missing".into(),
            },
        };
        let resource_path = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments("res", "tags", vec![step.clone()]),
        });
        let postfix_access = crate::resource::apply_postfix_access(
            Value::root_resource_ref("res".to_string(), "tags".to_string(), vec![]),
            step,
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(postfix_access, resource_path);

        let expected = Err(ResolutionError::Message(
            "res.tags['missing']: key not found; available keys: 'present'".to_string(),
        ));
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&resource_path, &bindings, session)
            ),
            expected
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&postfix_access, &bindings, session)
            ),
            expected
        );
    }

    #[test]
    fn resource_path_and_postfix_access_both_index_string_lists() {
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let bindings = bindings_from(vec![(
            "res",
            vec![(
                "names",
                Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()])),
            )],
        )]);
        let step = PathSegment::Subscript {
            index: Subscript::Int { index: 1 },
        };
        let resource_path = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments("res", "names", vec![step.clone()]),
        });
        let postfix_access = crate::resource::apply_postfix_access(
            Value::root_resource_ref("res".to_string(), "names".to_string(), vec![]),
            step,
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();

        assert_eq!(postfix_access, resource_path);

        let expected = Ok(Value::Concrete(ConcreteValue::String("b".into())));
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&resource_path, &bindings, session)
            ),
            expected
        );
        assert_eq!(
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| resolve_ref_value(&postfix_access, &bindings, session)
            ),
            expected
        );
    }

    #[test]
    fn local_reference_wrong_shape_is_never_deferred_as_a_missing_value() {
        use crate::resource::{AccessPath, PathSegment, Subscript};

        let bindings = bindings_from(vec![(
            "q",
            vec![(
                "identifier",
                Value::Concrete(ConcreteValue::String("q-123".into())),
            )],
        )]);
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments(
                "q",
                "identifier",
                vec![PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                }],
            ),
        });

        let error = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&value, &bindings, session),
        )
        .expect_err("a String cannot become indexable after apply");
        assert!(
            error
                .to_string()
                .contains("cannot apply access [0] to string"),
            "unexpected apply-time error: {error}",
        );
        assert!(
            !error.to_string().contains("wait"),
            "wrong-shape access must never be reported as a deferred miss: {error}",
        );
    }

    #[test]
    fn test_resolve_function_call_unresolved_ref_kept() {
        let bindings: ResolvedBindings = ResolvedBindings::default();

        // If a ResourceRef in the args can't be resolved, the FunctionCall is kept
        let func = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![Value::root_resource_ref(
                    "unknown".to_string(),
                    "id".to_string(),
                    vec![],
                )]))
                .into(),
            ],
            source_line: (0).into(),
        });

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&func, &bindings, session),
        )
        .unwrap();
        assert!(matches!(
            resolved,
            Value::Deferred(DeferredValue::FunctionCall { .. })
        ));
    }

    #[test]
    fn test_resolve_chained_field_access() {
        // web binding has a nested map: network = { vpc_id = "vpc-123" }
        let mut network_map = IndexMap::new();
        network_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("network", Value::Concrete(ConcreteValue::Map(network_map)))],
        )]);

        // web.network.vpc_id should resolve to "vpc-123"
        let ref_value = Value::root_resource_ref(
            "web".to_string(),
            "network".to_string(),
            vec!["vpc_id".to_string()],
        );

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-123".into()))
        );
    }

    #[test]
    fn test_resolve_deeply_chained_field_access() {
        // web.output.network.vpc_id
        let mut inner_map = IndexMap::new();
        inner_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-456".into())),
        );
        let mut output_map = IndexMap::new();
        output_map.insert(
            "network".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("output", Value::Concrete(ConcreteValue::Map(output_map)))],
        )]);

        let ref_value = Value::root_resource_ref(
            "web".to_string(),
            "output".to_string(),
            vec!["network".to_string(), "vpc_id".to_string()],
        );

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String("vpc-456".into()))
        );
    }

    #[test]
    fn test_resolve_chained_field_missing_key_keeps_ref() {
        let mut network_map = IndexMap::new();
        network_map.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );
        let bindings = bindings_from(vec![(
            "web",
            vec![("network", Value::Concrete(ConcreteValue::Map(network_map)))],
        )]);

        // A local desired-side view may not contain provider-populated fields yet.
        let ref_value = Value::root_resource_ref(
            "web".to_string(),
            "network".to_string(),
            vec!["nonexistent".to_string()],
        );

        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(resolved, ref_value);
    }

    #[test]
    fn resolve_builtin_error_propagated_when_args_resolved() {
        // env() with a var name that is extremely unlikely to be set should propagate error
        let bindings: ResolvedBindings = ResolvedBindings::default();
        let value = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![
                Value::Concrete(ConcreteValue::String(
                    "CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345"
                        .to_string()
                        .into(),
                ))
                .into(),
            ],
            source_line: (0).into(),
        });

        let result = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&value, &bindings, session),
        );
        assert!(
            result.is_err(),
            "Expected error for env() with missing var, got: {:?}",
            result
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg
                .to_string()
                .contains("CARINA_RESOLVER_TEST_NONEXISTENT_VAR_12345"),
            "Error should mention the missing env var, got: {}",
            err_msg
        );
    }

    #[test]
    fn resolve_builtin_with_unresolved_ref_stays_as_function_call() {
        // join("-", vpc.tags) should stay as FunctionCall when vpc.tags is unresolved
        let bindings: ResolvedBindings = ResolvedBindings::default();
        let value = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::root_resource_ref("vpc".to_string(), "tags".to_string(), vec![]).into(),
            ],
            source_line: (0).into(),
        });

        let result = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&value, &bindings, session),
        );
        assert!(result.is_ok(), "Unresolved ref should not cause error");
        match result.unwrap() {
            Value::Deferred(DeferredValue::FunctionCall { name, .. }) => assert_eq!(name, "join"),
            other => panic!("Expected FunctionCall, got: {:?}", other),
        }
    }

    #[test]
    fn resolve_refs_with_state_propagates_builtin_error() {
        let mut resources = vec![make_resource(
            "test",
            None,
            vec![(
                "value",
                Value::Deferred(DeferredValue::FunctionCall {
                    name: crate::builtins::BuiltinFunction::env,
                    args: vec![
                        Value::Concrete(ConcreteValue::String(
                            "CARINA_RESOLVER_STATE_TEST_NONEXISTENT_VAR_12345"
                                .to_string()
                                .into(),
                        ))
                        .into(),
                    ],
                    source_line: (0).into(),
                }),
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();
        let result = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                resolve_refs_with_state(
                    &mut resources,
                    &crate::resource::into_plan_input_current_map_from_states(
                        current_states.clone(),
                        &crate::schema::SchemaRegistry::new(),
                        &[],
                    ),
                    session,
                )
            },
        );
        assert!(
            result.is_err(),
            "Expected error from resolve_refs_with_state, got Ok"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg
                .to_string()
                .contains("CARINA_RESOLVER_STATE_TEST_NONEXISTENT_VAR_12345"),
            "Error should mention the missing env var, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_resolve_upstream_state_binding() {
        // Simulate a resource that references an upstream_state binding:
        // network.vpc.vpc_id where network is an upstream_state
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                // network.vpc.vpc_id -> ResourceRef { binding: "network", attribute: "vpc", field_path: ["vpc_id"] }
                Value::root_resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();

        // Build remote bindings: network -> { vpc -> Map { vpc_id -> "vpc-123" } }
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut vpc_attrs = IndexMap::new();
        vpc_attrs.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );
        vpc_attrs.insert(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
        let mut network_map = HashMap::new();
        network_map.insert(
            "vpc".to_string(),
            Value::Concrete(ConcreteValue::Map(vpc_attrs)),
        );
        remote_bindings.insert("network".to_string(), network_map);

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &current_states,
            &remote_bindings,
            &[],
        )
        .unwrap();

        assert_eq!(
            resources[0].get_attr("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String("vpc-123".into())))
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_substitutes_value() {
        // Issue #2435: `${orgs.accounts['registry_dev']}` against an
        // upstream that exports `accounts: map(AwsAccountId)` must
        // resolve to the concrete account id, not the literal substring
        // or an unresolved ref.
        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                // orgs.accounts["registry_dev"]
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_dev".into(),
                        }],
                    ),
                }),
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::Concrete(ConcreteValue::String("111111111111".into())),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::Concrete(ConcreteValue::String("222222222222".into())),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::Map(accounts_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .unwrap();

        assert_eq!(
            resources[0].get_attr("principal_arn"),
            Some(&Value::Concrete(ConcreteValue::String(
                "222222222222".into()
            ))),
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_missing_key_errors() {
        // Issue #2435 acceptance: when the upstream map IS loaded
        // (concrete `Value::Map`) but the requested string key is not
        // among its entries, the resolver must surface a clear error
        // naming the binding, attribute, and key — otherwise a typo
        // silently degrades to "(known after upstream apply: …)".
        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_qa".into(),
                        }],
                    ),
                }),
            )],
        )];

        let mut accounts_map: IndexMap<String, Value> = IndexMap::new();
        accounts_map.insert(
            "registry_prod".to_string(),
            Value::Concrete(ConcreteValue::String("111111111111".into())),
        );
        accounts_map.insert(
            "registry_dev".to_string(),
            Value::Concrete(ConcreteValue::String("222222222222".into())),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::Map(accounts_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let err = resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .expect_err("missing key in concrete upstream map must error");
        assert!(
            err.to_string().contains("registry_qa"),
            "error must name the missing key, got: {err}"
        );
        // Should also list available keys so the user can fix the typo.
        assert!(
            err.to_string().contains("registry_prod") && err.to_string().contains("registry_dev"),
            "error should list known keys, got: {err}"
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_empty_map_errors() {
        // Empty `Value::Map` exists but has no keys — the error must
        // still fire and the available-keys list must say so explicitly
        // ("<no keys>") rather than render an empty string.
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "registry_dev".into(),
                        }],
                    ),
                }),
            )],
        )];
        let err = resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .expect_err("empty map subscript must error");
        assert!(
            err.to_string().contains("<no keys>"),
            "empty-map error must say '<no keys>', got: {err}"
        );
    }

    #[test]
    fn test_resolve_upstream_state_map_subscript_unloaded_keeps_ref() {
        // The new missing-key error must NOT fire when the upstream
        // binding itself isn't loaded yet (e.g. plan-time before the
        // upstream apply has run). The resolver should keep the ref so
        // `resolve_refs_for_plan` can stamp it as
        // `Unknown(UpstreamExport)`. Guards against a future refactor that
        // hoists the subscript walk above the binding-presence check.
        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "principal_arn",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "orgs",
                        "accounts",
                        Vec::new(),
                        vec![crate::resource::Subscript::Str {
                            key: "anything".into(),
                        }],
                    ),
                }),
            )],
        )];

        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .expect("unloaded upstream subscript must not error");
        assert!(
            matches!(
                resources[0].get_attr("principal_arn"),
                Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
            ),
            "ref must stay as ResourceRef when upstream is not loaded"
        );
    }

    #[test]
    fn test_resolve_upstream_state_chained_subscripts() {
        // `orgs.regions['us'][0]` against a `map(list(String))` should
        // walk the map, then index into the list, returning the leaf.
        let inner_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("aza".into())),
            Value::Concrete(ConcreteValue::String("azb".into())),
        ]));
        let mut regions_map: IndexMap<String, Value> = IndexMap::new();
        regions_map.insert("us".to_string(), inner_list);
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "regions".to_string(),
            Value::Concrete(ConcreteValue::Map(regions_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "az",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "orgs",
                        "regions",
                        Vec::new(),
                        vec![
                            crate::resource::Subscript::Str { key: "us".into() },
                            crate::resource::Subscript::Int { index: 0 },
                        ],
                    ),
                }),
            )],
        )];
        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .unwrap();
        assert_eq!(
            resources[0].get_attr("az"),
            Some(&Value::Concrete(ConcreteValue::String("aza".into())))
        );
    }

    #[test]
    fn test_resolve_upstream_state_chained_subscript_missing_key_errors() {
        // `orgs.regions['eu'][0]` where map has only 'us' — the missing
        // string key surfaces the same key-not-found error as the
        // single-subscript case.
        let mut regions_map: IndexMap<String, Value> = IndexMap::new();
        regions_map.insert(
            "us".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("aza".into()),
            )])),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "regions".to_string(),
            Value::Concrete(ConcreteValue::Map(regions_map)),
        );
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        let mut resources = vec![make_resource(
            "policy",
            None,
            vec![(
                "az",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::root_with_fields_and_subscripts(
                        "orgs",
                        "regions",
                        Vec::new(),
                        vec![
                            crate::resource::Subscript::Str { key: "eu".into() },
                            crate::resource::Subscript::Int { index: 0 },
                        ],
                    ),
                }),
            )],
        )];
        let err = resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .expect_err("chained subscript with missing key must error");
        assert!(
            err.to_string().contains("eu") && err.to_string().contains("us"),
            "error must mention the missing key and known keys, got: {err}"
        );
    }

    #[test]
    fn test_resolve_upstream_state_unresolved_keeps_ref() {
        // If the upstream state doesn't have the referenced resource, the ref stays as-is
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::root_resource_ref(
                    "network".to_string(),
                    "nonexistent".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];

        let current_states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &current_states,
            &remote_bindings,
            &[],
        )
        .unwrap();

        // Should remain as ResourceRef since "nonexistent" binding is not found
        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
        ));
    }

    /// `resolve_refs_for_plan` stamps any surviving `ResourceRef` whose
    /// root binding is in `remote_bindings.keys()` as
    /// `Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport { path }))`.
    #[test]
    fn test_resolve_refs_for_plan_stamps_top_level_unresolved_upstream() {
        use crate::resource::ApplyTimeUnknown;
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::root_resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("vpc_id") {
            Some(crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamExport { path },
            ))) => {
                assert_eq!(path.to_dot_string(), "network.vpc.vpc_id");
            }
            other => panic!("expected Value::Unknown(UpstreamExport), got {:?}", other),
        }
    }

    /// The apply-side `resolve_refs_with_state_and_remote` must leave a
    /// surviving upstream `ResourceRef` intact (no marker String). Apply
    /// still requires every upstream value to be resolved at apply time;
    /// the resolver-level guarantee is "no stamping unless `for_plan`".
    #[test]
    fn test_resolve_refs_with_state_and_remote_leaves_unresolved_ref_intact() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::root_resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_with_state_and_remote_legacy(
            &mut resources,
            &HashMap::new(),
            &remote_bindings,
            &[],
        )
        .unwrap();

        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
        ));
    }

    /// Bare-binding refs into an upstream_state binding must also be
    /// stamped — `let v = bootstrap` (no `.attr`) parses as
    /// `Value::BindingRef { binding: "bootstrap" }` since #2856.
    /// Without a dedicated arm in `stamp_unresolved_upstream`, the
    /// `BindingRef` would slip past the marker and surface in plan
    /// display as a plain identifier instead of the
    /// `(known after upstream apply: bootstrap)` form. #2876.
    #[test]
    fn test_resolve_refs_for_plan_stamps_upstream_all_exports_ref() {
        use crate::resource::ApplyTimeUnknown;
        let mut resources = vec![make_resource(
            "consumer",
            None,
            vec![(
                "raw",
                Value::Deferred(DeferredValue::BindingRef {
                    binding: absolute_binding("bootstrap"),
                }),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("bootstrap".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("raw") {
            Some(Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports { binding },
            ))) => {
                assert_eq!(binding.as_str(), "bootstrap");
            }
            other => panic!(
                "expected Value::Unknown(UpstreamAllExports), got {:?}",
                other
            ),
        }
    }

    /// `UpstreamAllExports` unknowns nested inside a `List` must reach the
    /// stamping arm via the existing recursion. Locks in the recursive
    /// contract symmetrically with the `UpstreamExport`-inside-list test
    /// above. #2876.
    #[test]
    fn test_resolve_refs_for_plan_stamps_upstream_all_exports_inside_list() {
        use crate::resource::ApplyTimeUnknown;
        let mut resources = vec![make_resource(
            "consumer",
            None,
            vec![(
                "raws",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: absolute_binding("bootstrap"),
                    }),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: absolute_binding("secondary"),
                    }),
                ])),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("bootstrap".to_string(), HashMap::new());
        remote_bindings.insert("secondary".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("raws") {
            Some(crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(items))) => {
                assert_eq!(items.len(), 2);
                let bindings: Vec<&str> = items
                    .iter()
                    .map(|v| match v {
                        crate::resource::Value::Deferred(
                            crate::resource::DeferredValue::Unknown(
                                ApplyTimeUnknown::UpstreamAllExports { binding },
                            ),
                        ) => binding.as_str(),
                        other => panic!("expected UpstreamAllExports, got {:?}", other),
                    })
                    .collect();
                assert_eq!(bindings, vec!["bootstrap", "secondary"]);
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    /// Refs to non-upstream bindings must not be marked, so existing
    /// diagnostics for in-DSL `let` typos keep working.
    #[test]
    fn test_resolve_refs_for_plan_leaves_non_upstream_refs() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::root_resource_ref("main".to_string(), "vpc_id".to_string(), Vec::new()),
            )],
        )];
        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        assert!(matches!(
            resources[0].get_attr("vpc_id"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
        ));
    }

    /// Upstream refs nested inside a `List` must be reached too — e.g.
    /// `security_group_ids = [network.public_sg, network.private_sg]`.
    #[test]
    fn test_resolve_refs_for_plan_stamps_inside_list() {
        let mut resources = vec![make_resource(
            "instance",
            None,
            vec![(
                "security_group_ids",
                Value::Concrete(ConcreteValue::List(vec![
                    Value::root_resource_ref(
                        "network".to_string(),
                        "public_sg".to_string(),
                        Vec::new(),
                    ),
                    Value::root_resource_ref(
                        "network".to_string(),
                        "private_sg".to_string(),
                        Vec::new(),
                    ),
                ])),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("security_group_ids") {
            Some(Value::Concrete(ConcreteValue::List(items))) => {
                assert_eq!(items.len(), 2);
                for (idx, item) in items.iter().enumerate() {
                    assert!(
                        matches!(item, Value::Deferred(DeferredValue::Unknown(_))),
                        "list[{}] should be Value::Unknown, got {:?}",
                        idx,
                        item
                    );
                }
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    /// Stamping must not destroy the `dependency_bindings` saved by the
    /// resolver before stamping. Plan-tree building and `Delete` effect
    /// linkage rely on this metadata; without it, a deleted resource that
    /// referenced an unresolved upstream would lose the parent-child link.
    #[test]
    fn test_resolve_refs_for_plan_preserves_dependency_bindings() {
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![(
                "vpc_id",
                Value::root_resource_ref(
                    "network".to_string(),
                    "vpc".to_string(),
                    vec!["vpc_id".to_string()],
                ),
            )],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        assert!(
            resources[0]
                .dependencies
                .value_bindings
                .contains(&absolute_binding("network")),
            "dependency_bindings must record the upstream binding even after stamping"
        );
    }

    /// Upstream refs nested inside `Map` must be reached — a
    /// `tags = { id = network.vpc.vpc_id }` attribute would otherwise
    /// render the raw dot-form when only the top level gets stamped.
    #[test]
    fn test_resolve_refs_for_plan_recurses_into_collections() {
        let inner = Value::root_resource_ref(
            "network".to_string(),
            "vpc".to_string(),
            vec!["vpc_id".to_string()],
        );
        let mut tag_map: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        tag_map.insert("VpcId".to_string(), inner);
        let mut resources = vec![make_resource(
            "web-sg",
            None,
            vec![("tags", Value::Concrete(ConcreteValue::Map(tag_map)))],
        )];
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote_bindings.insert("network".to_string(), HashMap::new());

        resolve_refs_for_plan_legacy(&mut resources, &HashMap::new(), &remote_bindings, &[])
            .unwrap();

        match resources[0].get_attr("tags") {
            Some(crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(m))) => {
                match m.get("VpcId") {
                    Some(crate::resource::Value::Deferred(
                        crate::resource::DeferredValue::Unknown(_),
                    )) => {}
                    other => panic!("nested entry should be Value::Unknown, got {:?}", other),
                }
            }
            other => panic!("expected Map, got {:?}", other),
        }
    }

    /// carina#3085 design Test plan item 3: `resolve_ref_value` on a
    /// `<wait-binding>.<attr>` ResourceRef resolves through the
    /// materialised wait alias to the target's value — not left as an
    /// unresolved ref. Exercises the resolver layer directly (the
    /// real-pipeline E2E lives in `tests/wait_downstream_apply.rs`).
    #[test]
    fn resolve_ref_value_resolves_wait_binding_passthrough() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:1:certificate/abc".into(),
                )),
            )],
        );
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &[cert],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[crate::binding_index::WaitAliasSpec {
                binding: absolute_binding("cert_issued"),
                target: absolute_binding("cert"),
            }],
        });
        let path = crate::resource::AccessPath::root("cert_issued", "certificate_arn");
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:us-east-1:1:certificate/abc".into()
            )),
            "cert_issued.certificate_arn must resolve through the wait \
             alias to cert's value, not stay an unresolved ResourceRef"
        );
    }

    /// A wait binding whose target is absent: the ref is left intact
    /// (the existing `Ok(value.clone())` fallthrough), so the existing
    /// PlanError path — not a panic — surfaces it.
    #[test]
    fn resolve_ref_value_wait_binding_absent_target_left_intact() {
        let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
            managed: &[] as &[Resource],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[crate::binding_index::WaitAliasSpec {
                binding: absolute_binding("cert_issued"),
                target: absolute_binding("nonexistent"),
            }],
        });
        let path = crate::resource::AccessPath::root("cert_issued", "certificate_arn");
        let ref_value = Value::Deferred(DeferredValue::ResourceRef { path });
        let resolved = crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| resolve_ref_value(&ref_value, &bindings, session),
        )
        .unwrap();
        assert_eq!(
            resolved, ref_value,
            "no alias when target absent → ref stays intact (existing PlanError surfaces it)"
        );
    }
}

#[cfg(test)]
mod runtime_provider_type_tests {
    use super::*;
    use crate::parser::{OwnedTypeIdentity, ProviderContext, ProviderCustomTypes, parse_file};
    use crate::schema::{RefinedScalarKind, TypeIdentity};
    use crate::validation::deferred_types::ProviderTypeCheckDiagnostic;

    #[test]
    fn settlement_runtime_function_arguments_and_returns_use_catalog_authority() {
        for source in [
            "fn f(x: ready.ReadyId) { x }",
            "fn f(x: String): ready.ReadyId { x }",
        ] {
            let mut parsed = parse_file(
                source,
                std::path::Path::new("/project/functions.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            let mut function = parsed.user_functions.remove("f").unwrap();
            function.body.source_file = Some(std::path::PathBuf::from("/project/functions.crn"));
            let arguments = [crate::resource::FunctionArgument::from_value(
                crate::resource::AuthoredValue::Concrete(
                    crate::resource::AuthoredConcreteValue::String("bad".into()),
                ),
            )];
            let bindings = ResolvedBindings::for_authored_validation(&parsed);
            let loaded = ProviderContext {
                custom_types: ProviderCustomTypes::loaded(
                    std::collections::HashSet::from([OwnedTypeIdentity::provider(
                        "ready",
                        TypeIdentity::new(Some("ready"), Vec::<String>::new(), "ReadyId"),
                        RefinedScalarKind::String,
                    )]),
                    Box::new(|target, _| {
                        Err(crate::parser::fixture_custom_type_rejection(
                            target,
                            "rejected by ready",
                        ))
                    }),
                ),
                ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
            };
            let error =
                crate::validation::deferred_types::with_session_for_test(&loaded, |session| {
                    evaluate_runtime_user_function(&function, &arguments, &bindings, session, 8)
                })
                .unwrap_err();
            assert!(error.to_string().contains("rejected by ready"), "{error}");
            let partial = ProviderContext {
                custom_types: loaded
                    .custom_types
                    .with_failed_providers(std::collections::BTreeSet::from(["broken".into()])),
                ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
            };
            let error =
                crate::validation::deferred_types::with_session_for_test(&partial, |session| {
                    evaluate_runtime_user_function(&function, &arguments, &bindings, session, 8)
                })
                .unwrap_err();
            assert!(error.to_string().contains("rejected by ready"), "{error}");

            let result = crate::validation::deferred_types::TypeCheckSession::run(
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                |unloaded| {
                    evaluate_runtime_user_function(&function, &arguments, &bindings, unloaded, 8)
                        .unwrap();
                },
            );
            assert!(
                matches!(result.diagnostics(), [ProviderTypeCheckDiagnostic::Deferred(warning)] if warning.file.as_deref() == Some("/project/functions.crn")),
                "{:?}",
                result.diagnostics()
            );
        }
    }
}
