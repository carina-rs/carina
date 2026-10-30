//! Single-effect execution: Create, Update, Delete dispatch, resource resolution,
//! Secret unwrapping, and post-apply binding updates.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::binding_index::{ResolvedBindings, SecretPreservingAttributes};
use crate::differ::{
    AttrComparison, LeafProvenance, TypedAttr, key_should_enter_patch,
    secret_grafted_comparison_view,
};
use crate::effect::{BasicEffect, DeletedInstanceKey, Effect, EffectGeneration};
use crate::executor::provider_ready::{
    CheckInput, ProviderPreparationContext, ResolvedCheckInput, managed_update_schema,
    prepare_provider_ready_resource_after_resolution,
};
use crate::provider::{
    AppliedIdentifier, CompletedResume, ContextualProvider, CreateFailureKind, CreateRequest,
    DeleteRequest, PartialReadDiagnostic, Provider, ProviderReadyResource, ReadRequest,
    ResumeCreateRequest, ResumeFailureKind, ResumeOutcomeKind, UpdateOutcome, UpdateRequest,
};
use crate::resolver::resolve_ref_value;
use crate::resource::{
    ConcreteValue, Current, DataSource, DeferredValue, Resource, ResourceId, State, Value,
};
use crate::value::{SerializationContext, SerializationError};

use super::wait::{InFlightStart, StartedEffect};
use super::{
    AcceptedRequest, Applied, ExecutionAccumulator, ExecutionCheckpointSink, ExecutionEvent,
    ExecutionObserver, ProgressInfo,
};

pub(super) struct CompletedEffect {
    state: Box<State>,
    request: AcceptedRequest,
}

/// Result of executing a basic effect (Create, Update, or Delete).
///
/// This is the shared result type used by `execute_basic_effect` to avoid
/// duplicating effect dispatch logic across sequential and phased paths.
pub(super) enum BasicEffectResult {
    Success {
        completed: CompletedEffect,
        resource_id: ResourceId,
        resolved_attrs: Option<SecretPreservingAttributes>,
        binding: Option<String>,
    },
    PartialSuccess {
        completed: CompletedEffect,
        resource_id: ResourceId,
        diagnostic: PartialReadDiagnostic,
        resolved_attrs: Option<SecretPreservingAttributes>,
        binding: Option<String>,
    },
    Failure {
        error: String,
        refresh: Option<(ResourceId, AppliedIdentifier)>,
    },
    ResumeSuccess {
        completed: CompletedResume,
        resource_id: ResourceId,
        resolved_attrs: SecretPreservingAttributes,
        binding: Option<String>,
    },
    ClassifiedFailure {
        error: String,
        resource_id: ResourceId,
        applied: Applied,
    },
    Deleted {
        instance_key: DeletedInstanceKey,
    },
}

/// A completed basic provider future that still owns the obligation to emit
/// its one terminal effect event.
pub(super) struct StartedBasicEffectResult<'a, S> {
    started: StartedEffect<'a, S>,
    duration: Duration,
    outcome: BasicEffectResult,
}

impl<'a, S> StartedBasicEffectResult<'a, S> {
    fn new(started: StartedEffect<'a, S>, began: Instant, outcome: BasicEffectResult) -> Self {
        Self {
            started,
            duration: began.elapsed(),
            outcome,
        }
    }

    fn emit_terminal(self) -> BasicEffectResult {
        let Self {
            started,
            duration,
            outcome,
        } = self;
        match &outcome {
            BasicEffectResult::Success { completed, .. } => {
                started.succeeded(Some(&completed.state), duration)
            }
            BasicEffectResult::PartialSuccess {
                completed,
                diagnostic,
                ..
            } => started.partially_succeeded(&completed.state, diagnostic, duration),
            BasicEffectResult::Failure { error, .. }
            | BasicEffectResult::ClassifiedFailure { error, .. } => {
                started.failed(error, duration);
            }
            BasicEffectResult::ResumeSuccess { completed, .. } => {
                let state = completed.observed_state();
                started.succeeded(Some(&state), duration);
            }
            BasicEffectResult::Deleted { .. } => {
                started.succeeded(None, duration);
            }
        }
        outcome
    }
}

/// Mutable execution state shared across effect processing.
///
/// Groups the counters, result maps, and binding state that are threaded
/// through every call to `process_basic_result`, reducing parameter count.
pub(super) struct ExecutionState<'a, S> {
    pub(super) idx: usize,
    pub(super) accumulator: &'a mut ExecutionAccumulator<S>,
    pub(super) failed_indices: &'a mut std::collections::HashSet<usize>,
    pub(super) pending_refreshes: &'a mut HashMap<ResourceId, AppliedIdentifier>,
}

/// Queue a state refresh for a resource after a failed operation.
pub(super) fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, AppliedIdentifier>,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
) {
    pending_refreshes.insert(id.clone(), identifier.clone());
}

/// Refresh states for resources whose operations failed.
pub(super) async fn refresh_pending_states<S: Clone>(
    provider: &dyn Provider,
    accumulator: &mut ExecutionAccumulator<S>,
    pending_refreshes: &HashMap<ResourceId, AppliedIdentifier>,
    observer: &dyn ExecutionObserver<S>,
    checkpoint_sink: &ExecutionCheckpointSink<S>,
) {
    if pending_refreshes.is_empty() {
        return;
    }

    observer.on_event(&ExecutionEvent::RefreshStarted);

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by_key(|(left_id, _)| left_id.to_string());
    for (id, identifier) in refreshes {
        match provider.read(id, identifier, ReadRequest).await {
            Ok(outcome) => {
                observer.on_event(&ExecutionEvent::RefreshSucceeded { id });
                accumulator.mutate(|result| {
                    result
                        .managed_current
                        .insert(id.clone(), Current::from_state(outcome.into_state()));
                });
            }
            Err(error) => {
                let error_str = error.to_string();
                observer.on_event(&ExecutionEvent::RefreshFailed {
                    id,
                    error: &error_str,
                });
                accumulator.mutate(|result| {
                    result.failed_refreshes.insert(id.clone());
                });
            }
        }
        accumulator.publish_if_dirty(checkpoint_sink);
    }
}

/// Resolve a resource's attributes using the current bindings.
/// Secret values are unwrapped so the provider receives the plain inner value.
///
/// Any attribute that still contains a `Value::Deferred(ResourceRef|
/// BindingRef|Interpolation|FunctionCall)` after resolution is rejected
/// here — pre-#3032 the executor passed such values straight through
/// to the provider, where the WASM serializer surfaced them as a
/// generic "cannot serialize at WASM provider boundary" error with no
/// hint about *why* the reference was unresolved.
///
/// The fail-fast happens at the executor seam rather than at the WASM
/// boundary so the error names the attribute and points the user at
/// `wait` as the synchronization mechanism for upstream attributes
/// that populate asynchronously (ACM `domain_validation_options`,
/// CloudFront `domain_name`, etc.).
pub(super) async fn resolve_resource<S: Clone>(
    input: Result<CheckInput<Resource<S>>, String>,
    context: &ProviderPreparationContext<'_>,
) -> Result<(ProviderReadyResource, SecretPreservingAttributes), String> {
    let bindings = context.bindings();
    let input = input?;
    let checked = match input {
        CheckInput::Authored(source) => {
            let mut resolved = source.clone();
            for (key, expr) in &source.attributes {
                let resolved_value = resolve_ref_value(expr, bindings)?;
                assert_fully_resolved(&resolved_value, key, bindings)?;
                resolved.attributes.insert(key.clone(), resolved_value);
            }
            ResolvedCheckInput::authored(resolved)
        }
        CheckInput::PlanNormalized {
            resolved: target,
            authored: source,
        } => {
            let mut resolved = target.clone();
            let mut validation_attributes = source.resolved_attributes();
            let mut apply_resolved_attributes = HashSet::new();
            for (key, target_expr) in &target.attributes {
                let source_expr = source.attributes.get(key);
                let resolves_at_apply = source_expr.is_some_and(needs_apply_resolution);
                let expr = source_expr
                    .filter(|source_expr| needs_apply_resolution(source_expr))
                    .unwrap_or(target_expr);
                let resolved_value = resolve_ref_value(expr, bindings)?;
                assert_fully_resolved(&resolved_value, key, bindings)?;
                if resolves_at_apply {
                    validation_attributes.insert(key.clone(), resolved_value.clone());
                    apply_resolved_attributes.insert(key.clone());
                }
                resolved.attributes.insert(key.clone(), resolved_value);
            }
            for (key, source_expr) in &source.attributes {
                if target.attributes.contains_key(key) {
                    continue;
                }
                let resolved_value = resolve_ref_value(source_expr, bindings)?;
                assert_fully_resolved(&resolved_value, key, bindings)?;
                if needs_apply_resolution(source_expr) {
                    validation_attributes.insert(key.clone(), resolved_value.clone());
                    apply_resolved_attributes.insert(key.clone());
                }
                resolved.attributes.insert(key.clone(), resolved_value);
            }
            ResolvedCheckInput::plan_normalized(
                resolved,
                validation_attributes,
                apply_resolved_attributes,
            )
        }
    };
    let resolved_secret_shapes = checked.value().resolved_attributes();
    let provider_ready = prepare_provider_ready_resource_after_resolution(checked, context)
        .await
        .map_err(|err| err.to_string())?;
    let provider_values = provider_ready.as_resource().resolved_attributes();
    let resolved_desired_attributes =
        SecretPreservingAttributes::from_provider_values(&resolved_secret_shapes, &provider_values);
    Ok((provider_ready, resolved_desired_attributes))
}

fn needs_apply_resolution(value: &Value) -> bool {
    match value {
        Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => true,
        Value::Deferred(DeferredValue::Secret(inner)) => needs_apply_resolution(inner),
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(needs_apply_resolution),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(needs_apply_resolution),
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        ) => false,
    }
}

pub(super) fn resolve_data_source<S: Clone>(
    input: Result<CheckInput<DataSource<S>>, String>,
    bindings: &ResolvedBindings,
) -> Result<ResolvedCheckInput<DataSource<S>>, String> {
    let checked = match input? {
        CheckInput::Authored(source) => {
            let mut resolved = source.clone();
            for (key, expr) in &source.attributes {
                let resolved_value = resolve_ref_value(expr, bindings)?;
                assert_fully_resolved(&resolved_value, key, bindings)?;
                resolved.attributes.insert(key.clone(), resolved_value);
            }
            ResolvedCheckInput::authored(resolved)
        }
        CheckInput::PlanNormalized {
            resolved: target,
            authored: source,
        } => {
            let mut resolved = target.clone();
            let mut validation_attributes = crate::resource::attrs_to_hashmap(&source.attributes);
            let mut apply_resolved_attributes = HashSet::new();
            for (key, target_expr) in &target.attributes {
                let source_expr = source.attributes.get(key);
                let resolves_at_apply = source_expr.is_some_and(needs_apply_resolution);
                let expr = source_expr
                    .filter(|source_expr| needs_apply_resolution(source_expr))
                    .unwrap_or(target_expr);
                let resolved_value = resolve_ref_value(expr, bindings)?;
                assert_fully_resolved(&resolved_value, key, bindings)?;
                if resolves_at_apply {
                    validation_attributes.insert(key.clone(), resolved_value.clone());
                    apply_resolved_attributes.insert(key.clone());
                }
                resolved.attributes.insert(key.clone(), resolved_value);
            }
            for (key, source_expr) in &source.attributes {
                if target.attributes.contains_key(key) {
                    continue;
                }
                let resolved_value = resolve_ref_value(source_expr, bindings)?;
                assert_fully_resolved(&resolved_value, key, bindings)?;
                if needs_apply_resolution(source_expr) {
                    validation_attributes.insert(key.clone(), resolved_value.clone());
                    apply_resolved_attributes.insert(key.clone());
                }
                resolved.attributes.insert(key.clone(), resolved_value);
            }
            ResolvedCheckInput::plan_normalized(
                resolved,
                validation_attributes,
                apply_resolved_attributes,
            )
        }
    };
    Ok(checked)
}

#[cfg(test)]
pub(super) fn resolved_resource(
    resource: Resource,
) -> Result<crate::resource::ResolvedResource, SerializationError> {
    crate::resource::assert_resource_fully_resolved(&resource)?;
    Ok(crate::resource::ResolvedResource::new(resource))
}

/// Reject a resolved attribute value that still carries an unresolved
/// `Deferred` payload (carina#3032).
///
/// Walks `List` / `Map` containers so a chained `[idx].field` ref
/// hidden inside `resource_records = [cert.dvo[0].rrv]` is caught.
/// `Secret` is peeled and recursed — secret-tagged refs still need to
/// be resolved before reaching the provider (the executor unwraps
/// secrets just downstream of this check).
///
fn assert_fully_resolved(
    value: &Value,
    attribute_key: &str,
    bindings: &ResolvedBindings,
) -> Result<(), String> {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                assert_fully_resolved(item, attribute_key, bindings)?;
            }
            Ok(())
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                assert_fully_resolved(v, attribute_key, bindings)?;
            }
            Ok(())
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            assert_fully_resolved(inner, attribute_key, bindings)
        }
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Err(format!(
            "Attribute '{}' contains a saved-plan secret that was not rehydrated; create a new saved plan before applying",
            attribute_key
        )),
        Value::Deferred(DeferredValue::ResourceRef { path }) => Err(unresolved_binding_message(
            attribute_key,
            UnresolvedSite::Ref {
                path: path.to_dot_string(),
            },
            path.binding(),
            bindings,
        )),
        Value::Deferred(DeferredValue::BindingRef { binding }) => Err(unresolved_binding_message(
            attribute_key,
            UnresolvedSite::Ref {
                path: binding.clone(),
            },
            binding,
            bindings,
        )),
        Value::Deferred(DeferredValue::Coalesce { .. }) => {
            Err(SerializationError::UnresolvedCoalesce {
                context: SerializationContext::WasmBoundary,
            }
            .to_string())
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            let leaf = pick_unresolved_binding_for_diagnostic(value, bindings);
            Err(unresolved_binding_message(
                attribute_key,
                UnresolvedSite::Interpolation,
                leaf.unwrap_or(""),
                bindings,
            ))
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, .. } | DeferredValue::Unavailable { name, .. },
        ) => {
            let leaf = pick_unresolved_binding_for_diagnostic(value, bindings);
            Err(unresolved_binding_message(
                attribute_key,
                UnresolvedSite::FunctionCall {
                    name: name.to_string(),
                },
                leaf.unwrap_or(""),
                bindings,
            ))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            let leaf = pick_unresolved_binding_for_diagnostic(value, bindings);
            Err(unresolved_binding_message(
                attribute_key,
                UnresolvedSite::FunctionCall {
                    name: function.name.clone(),
                },
                leaf.unwrap_or(""),
                bindings,
            ))
        }
        Value::Deferred(DeferredValue::Access(_)) => {
            let leaf = pick_unresolved_binding_for_diagnostic(value, bindings);
            Err(unresolved_binding_message(
                attribute_key,
                UnresolvedSite::Access {
                    expression: crate::value::format_value(value),
                },
                leaf.unwrap_or(""),
                bindings,
            ))
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context: SerializationContext::WasmBoundary,
            }
            .to_string())
        }
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_)) => Ok(()),
    }
}

enum UnresolvedSite {
    Ref { path: String },
    Interpolation,
    FunctionCall { name: String },
    Access { expression: String },
}

fn unresolved_binding_message(
    attribute_key: &str,
    site: UnresolvedSite,
    referenced_binding: &str,
    bindings: &ResolvedBindings,
) -> String {
    let opener = match &site {
        UnresolvedSite::Ref { path } => format!(
            "attribute `{attribute_key}` references `{path}` which has not been published yet by the upstream binding."
        ),
        UnresolvedSite::Interpolation => format!(
            "attribute `{attribute_key}` contains an interpolation that did not fully resolve at apply time."
        ),
        UnresolvedSite::FunctionCall { name } => format!(
            "attribute `{attribute_key}` calls function `{name}()` whose arguments did not fully resolve at apply time."
        ),
        UnresolvedSite::Access { expression } => format!(
            "attribute `{attribute_key}` contains access expression `{expression}` that did not fully resolve at apply time."
        ),
    };

    let remediation = match bindings.source(referenced_binding) {
        Some(crate::binding_index::BindingValueSource::Upstream) => format!(
            "Binding `{referenced_binding}` comes from an `upstream_state` block, but the referenced export is not present in the upstream's saved state. \
             Apply the upstream stack first, then re-run apply here. \
             A `wait` block cannot be used here — `wait` only synchronizes on attributes of managed resources in this configuration."
        ),
        _ => "At least one referenced binding has not been published yet; add a `wait` block on the upstream binding to synchronize before downstream resources read it.".to_string(),
    };

    format!("{opener} {remediation}")
}

fn collect_unresolved_bindings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    use crate::resource::InterpolationPart;
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => out.push(path.binding()),
        Value::Deferred(DeferredValue::BindingRef { binding }) => out.push(binding.as_str()),
        Value::Deferred(DeferredValue::Secret(inner)) => collect_unresolved_bindings(inner, out),
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {}
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for p in parts {
                if let InterpolationPart::Expr(v) = p {
                    collect_unresolved_bindings(v, out);
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for a in args {
                collect_unresolved_bindings(a.walker_value(), out);
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            collect_unresolved_bindings(access.base(), out)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            collect_unresolved_bindings(value, out);
            collect_unresolved_bindings(default, out);
        }
        Value::Deferred(DeferredValue::Unknown(_)) => {}
        Value::Concrete(ConcreteValue::List(items)) => {
            for v in items {
                collect_unresolved_bindings(v, out);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                collect_unresolved_bindings(v, out);
            }
        }
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_)) => {}
    }
}

fn pick_unresolved_binding_for_diagnostic<'a>(
    value: &'a Value,
    bindings: &ResolvedBindings,
) -> Option<&'a str> {
    let mut names = Vec::new();
    collect_unresolved_bindings(value, &mut names);
    names
        .iter()
        .copied()
        .find(|name| {
            matches!(
                bindings.source(name),
                Some(crate::binding_index::BindingValueSource::Upstream)
            )
        })
        .or_else(|| names.first().copied())
}

/// Process a `BasicEffectResult` by updating shared execution state.
///
/// This helper is used by both sequential and phased execution paths to avoid
/// duplicating the result-processing logic for Create/Update/Delete effects.
pub(super) fn process_basic_result<S: Clone>(
    result: StartedBasicEffectResult<'_, S>,
    exec: &mut ExecutionState<'_, S>,
) {
    process_basic_outcome(result.emit_terminal(), exec);
}

fn process_basic_outcome<S: Clone>(result: BasicEffectResult, exec: &mut ExecutionState<'_, S>) {
    let idx = exec.idx;
    let failed_indices = &mut *exec.failed_indices;
    let pending_refreshes = &mut *exec.pending_refreshes;
    match result {
        BasicEffectResult::Success {
            completed,
            resource_id,
            resolved_attrs,
            binding,
        } => {
            let CompletedEffect { state, request } = completed;
            exec.accumulator.mutate(|accumulated| {
                accumulated.success_count += 1;
                if let Some(attrs) = &resolved_attrs {
                    accumulated
                        .bindings
                        .record_applied(binding.as_deref(), attrs, &state);
                }
            });
            let state = *state;
            exec.accumulator.insert_published(
                resource_id,
                Applied::Complete {
                    state: state.clone(),
                    request,
                },
                &state,
            );
        }
        BasicEffectResult::Failure { error: _, refresh } => {
            exec.accumulator
                .mutate(|accumulated| accumulated.failure_count += 1);
            failed_indices.insert(idx);
            if let Some((id, identifier)) = &refresh {
                queue_state_refresh(pending_refreshes, id, identifier);
            }
        }
        BasicEffectResult::ResumeSuccess {
            completed,
            resource_id,
            resolved_attrs,
            binding,
        } => {
            let state = completed.observed_state();
            exec.accumulator.mutate(|accumulated| {
                accumulated.success_count += 1;
                accumulated
                    .bindings
                    .record_applied(binding.as_deref(), &resolved_attrs, &state);
            });
            exec.accumulator.insert_published(
                resource_id,
                Applied::Observed(std::sync::Arc::new(completed.into_observed_create())),
                &state,
            );
        }
        BasicEffectResult::ClassifiedFailure {
            error: _,
            resource_id,
            applied,
        } => {
            exec.accumulator
                .mutate(|accumulated| accumulated.failure_count += 1);
            failed_indices.insert(idx);
            exec.accumulator.insert_unpublished(resource_id, applied);
        }
        BasicEffectResult::PartialSuccess {
            completed: CompletedEffect { state, request },
            resource_id,
            diagnostic,
            resolved_attrs,
            binding,
        } => {
            exec.accumulator.mutate(|accumulated| {
                accumulated.partial_count += 1;
                if let Some(attrs) = &resolved_attrs {
                    accumulated
                        .bindings
                        .record_applied(binding.as_deref(), attrs, &state);
                }
                accumulated
                    .partial_diagnostics
                    .push((resource_id.clone(), diagnostic));
            });
            let state = *state;
            exec.accumulator.insert_published(
                resource_id,
                Applied::Complete {
                    state: state.clone(),
                    request,
                },
                &state,
            );
        }
        BasicEffectResult::Deleted { instance_key } => {
            exec.accumulator.mutate(|accumulated| {
                accumulated.success_count += 1;
                accumulated.successfully_deleted.insert(instance_key);
            });
        }
    }
}

/// Count the number of actionable effects (excluding Read and state operations).
pub(super) fn count_actionable_effects<S>(effects: &[Effect<S>]) -> usize {
    effects
        .iter()
        .filter(|e| {
            !matches!(e, Effect::Read { .. }) && !e.is_state_operation() && !e.is_scheduler_meta()
        })
        .count()
}

/// Resolution + dispatch context for a basic effect, bundled to keep
/// `execute_basic_effect`'s arity in check (clippy `too_many_arguments`)
/// and keeping dispatch context fields grouped consistently.
pub(super) struct BasicEffectCtx<'a, S> {
    pub(super) provider: &'a dyn ContextualProvider,
    pub(super) provider_check_inputs: &'a super::ProviderCheckInputs<'a, S>,
    pub(super) runtime_authored: bool,
    pub(super) preparation: &'a ProviderPreparationContext<'a>,
    pub(super) resume_create: Option<crate::provider::InFlightCreate>,
    pub(super) completed: &'a AtomicUsize,
    pub(super) total: usize,
}

/// Execute a single Create, Update, or Delete effect.
///
/// Takes a [`BasicEffect`] — a type-level narrowing of `Effect` to the
/// three variants this function actually handles. Non-basic variants
/// (`Replace`/`Read`/`Import`/`Remove`/`Move`/`Wait`) cannot reach this
/// function because [`Effect::as_basic`] is the only constructor and
/// returns `None` for them. This contract used to live in caller-side
/// filters with an `unreachable!()` backstop; a missed filter
/// (carina#3164) panicked apply with `execute_basic_effect called
/// with non-basic effect`. The type now enforces it.
///
/// Returns a started result whose token must be consumed to emit the terminal
/// event before callers process its state changes.
pub(super) async fn execute_basic_effect<'effect, 'observer, S: Clone>(
    basic: BasicEffect<'effect, S>,
    ctx: &BasicEffectCtx<'_, S>,
    in_flight_start: InFlightStart<S>,
    observer: &'observer dyn ExecutionObserver<S>,
) -> StartedBasicEffectResult<'observer, S> {
    let provider = ctx.provider.provider();
    let preparation = ctx.preparation;
    let completed = ctx.completed;
    let total = ctx.total;
    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();
    let progress = ProgressInfo {
        completed: c,
        total,
    };
    let effect = basic.as_effect();
    let mut started_effect = in_flight_start.emit_started(observer, effect, progress);

    let outcome = match basic {
        BasicEffect::Create { resource, .. } => {
            let check_input = ctx
                .provider_check_inputs
                .resource_input(resource.as_inner(), ctx.runtime_authored);
            let (resolved, resolved_attrs) = match resolve_resource(check_input, preparation).await
            {
                Ok(r) => r,
                Err(e) => {
                    return StartedBasicEffectResult::new(
                        started_effect,
                        started,
                        BasicEffectResult::Failure {
                            error: e,
                            refresh: None,
                        },
                    );
                }
            };
            let (attempt, cancellation) =
                crate::provider::CreateAttempt::with_cancellation_report(resource.id.clone());
            started_effect.track_create_cancellation(cancellation);
            match super::dispatch_create_with_attempt(
                provider,
                &resource.id,
                CreateRequest::checked(resolved),
                attempt,
            )
            .await
            {
                Ok(outcome) => {
                    let (state, request, diagnostic) =
                        super::complete_create_for_writeback(outcome);
                    let request = request.with_resolved_desired_attributes(resolved_attrs.clone());
                    if let Some(diagnostic) = diagnostic {
                        BasicEffectResult::PartialSuccess {
                            completed: CompletedEffect {
                                state: Box::new(state),
                                request,
                            },
                            resource_id: resource.id.clone(),
                            diagnostic,
                            resolved_attrs: Some(resolved_attrs),
                            binding: resource.binding.clone(),
                        }
                    } else {
                        BasicEffectResult::Success {
                            completed: CompletedEffect {
                                state: Box::new(state),
                                request,
                            },
                            resource_id: resource.id.clone(),
                            resolved_attrs: Some(resolved_attrs),
                            binding: resource.binding.clone(),
                        }
                    }
                }
                Err(failure) => match failure.into_kind() {
                    CreateFailureKind::Untracked(error) => {
                        let error_str = error.to_string();
                        BasicEffectResult::Failure {
                            error: error_str,
                            refresh: None,
                        }
                    }
                    CreateFailureKind::OutcomeUnknown { unknown, error } => {
                        let error_str = format!(
                            "{error}\nthe create may have reached the cloud; import the resource if it exists, or forget the protected state row if it does not"
                        );
                        BasicEffectResult::ClassifiedFailure {
                            error: error_str,
                            resource_id: resource.id.clone(),
                            applied: Applied::CreateOutcomeUnknown(unknown),
                        }
                    }
                    CreateFailureKind::Created { created, error } => {
                        let created =
                            created.with_resolved_desired_attributes(resolved_attrs.clone());
                        let error_str = format!("{error}\n{}", created.post_error_note());
                        BasicEffectResult::ClassifiedFailure {
                            error: error_str,
                            resource_id: resource.id.clone(),
                            applied: Applied::Observed(created.into()),
                        }
                    }
                    CreateFailureKind::InFlight { pending, error } => {
                        let error_str =
                            format!("{error}\ncreate still in progress; the next apply resumes it");
                        BasicEffectResult::ClassifiedFailure {
                            error: error_str,
                            resource_id: resource.id.clone(),
                            applied: Applied::CreateInFlight(pending),
                        }
                    }
                },
            }
        }
        BasicEffect::ResumeCreate { resource, .. } => {
            let check_input = ctx
                .provider_check_inputs
                .resource_input(resource.as_inner(), ctx.runtime_authored);
            let (resolved, resolved_attrs) = match resolve_resource(check_input, preparation).await
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    return StartedBasicEffectResult::new(
                        started_effect,
                        started,
                        BasicEffectResult::Failure {
                            error,
                            refresh: None,
                        },
                    );
                }
            };
            let pending = match &ctx.resume_create {
                Some(pending) => pending.clone(),
                None => {
                    let error = format!(
                        "saved create continuation for {} changed before dispatch",
                        resource.id
                    );
                    return StartedBasicEffectResult::new(
                        started_effect,
                        started,
                        BasicEffectResult::Failure {
                            error,
                            refresh: None,
                        },
                    );
                }
            };
            let request = match ResumeCreateRequest::new(pending, CreateRequest::checked(resolved))
            {
                Ok(request) => request,
                Err(error) => {
                    let error = error.to_string();
                    return StartedBasicEffectResult::new(
                        started_effect,
                        started,
                        BasicEffectResult::Failure {
                            error,
                            refresh: None,
                        },
                    );
                }
            };
            match super::dispatch_resume_create(provider, request)
                .await
                .map(crate::provider::ResumeOutcome::into_kind)
            {
                Ok(ResumeOutcomeKind::Resumed(completed)) => {
                    let completed =
                        completed.with_resolved_desired_attributes(resolved_attrs.clone());
                    BasicEffectResult::ResumeSuccess {
                        completed,
                        resource_id: resource.id.clone(),
                        resolved_attrs,
                        binding: resource.binding.clone(),
                    }
                }
                Ok(ResumeOutcomeKind::Resent(outcome)) => {
                    let (state, request, diagnostic) =
                        super::complete_create_for_writeback(*outcome);
                    let request = request.with_resolved_desired_attributes(resolved_attrs.clone());
                    if let Some(diagnostic) = diagnostic {
                        BasicEffectResult::PartialSuccess {
                            completed: CompletedEffect {
                                state: Box::new(state),
                                request,
                            },
                            resource_id: resource.id.clone(),
                            diagnostic,
                            resolved_attrs: Some(resolved_attrs),
                            binding: resource.binding.clone(),
                        }
                    } else {
                        BasicEffectResult::Success {
                            completed: CompletedEffect {
                                state: Box::new(state),
                                request,
                            },
                            resource_id: resource.id.clone(),
                            resolved_attrs: Some(resolved_attrs),
                            binding: resource.binding.clone(),
                        }
                    }
                }
                Err(failure) => match failure.into_kind() {
                    ResumeFailureKind::Ended(error) => {
                        let error = error.to_string();
                        BasicEffectResult::ClassifiedFailure {
                            error,
                            resource_id: resource.id.clone(),
                            applied: Applied::CreateEnded,
                        }
                    }
                    ResumeFailureKind::Created { created, error } => {
                        let created =
                            created.with_resolved_desired_attributes(resolved_attrs.clone());
                        let error = format!("{error}\n{}", created.post_error_note());
                        BasicEffectResult::ClassifiedFailure {
                            error,
                            resource_id: resource.id.clone(),
                            applied: Applied::Observed(created.into()),
                        }
                    }
                    ResumeFailureKind::InFlight { pending, error } => {
                        let error =
                            format!("{error}\ncreate still in progress; the next apply resumes it");
                        BasicEffectResult::ClassifiedFailure {
                            error,
                            resource_id: resource.id.clone(),
                            applied: Applied::CreateInFlight(pending),
                        }
                    }
                },
            }
        }
        BasicEffect::Update {
            from,
            identifier,
            to,
            changed_attributes,
            ..
        } => {
            let id = &to.id;
            let check_input = ctx
                .provider_check_inputs
                .resource_input(to.as_inner(), ctx.runtime_authored);
            let resolve_source = check_input
                .as_ref()
                .ok()
                .and_then(|input| match input {
                    CheckInput::Authored(_) => None,
                    CheckInput::PlanNormalized { authored, .. } => Some(authored.clone()),
                })
                .unwrap_or_else(|| to.as_inner().clone());
            let (resolved_to, resolved_desired_attributes) =
                match resolve_resource(check_input, preparation).await {
                    Ok(r) => r,
                    Err(e) => {
                        return StartedBasicEffectResult::new(
                            started_effect,
                            started,
                            BasicEffectResult::Failure {
                                error: e,
                                refresh: None,
                            },
                        );
                    }
                };
            // Augment plan-time `changed_attributes` with any
            // ResourceRef-derived attributes whose resolved value at
            // apply time differs from `from`. This catches the case
            // where a dependency was just replaced (and the binding
            // map flipped to a new computed value) but the plan-time
            // diff did not see the change because both the old and
            // new ResourceRef expressions are syntactically identical.
            // Without this, the patch would omit the changed
            // reference value and the provider would never be told
            // to update it.
            let mut effective_changed: Vec<String> = changed_attributes.to_vec();
            let resolved_resource = resolved_to.as_resource();
            let schema = match managed_update_schema(preparation.schemas(), resolved_resource) {
                Ok(schema) => schema,
                Err(error) => {
                    let error = error.to_string();
                    return StartedBasicEffectResult::new(
                        started_effect,
                        started,
                        BasicEffectResult::Failure {
                            error,
                            refresh: None,
                        },
                    );
                }
            };
            let owner_schema = preparation
                .schemas()
                .get_for(resolved_resource)
                .expect("managed_update_schema proved that the owner-bound schema exists");
            let comparison = match ctx.provider.comparison_context_for(id, owner_schema) {
                Ok(comparison) => comparison,
                Err(error) => {
                    return StartedBasicEffectResult::new(
                        started_effect,
                        started,
                        BasicEffectResult::Failure {
                            error: error.to_string(),
                            refresh: None,
                        },
                    );
                }
            };
            for (key, new_value) in &resolved_resource.attributes {
                if effective_changed.iter().any(|k| k == key) {
                    continue;
                }
                let type_info = schema.attributes.get(key).map(|attr| TypedAttr {
                    attr_type: &attr.attr_type,
                    defs: &schema.defs,
                });
                let Some(comparison_value) =
                    secret_grafted_comparison_view(new_value, resolve_source.attributes.get(key))
                else {
                    continue;
                };
                // Apply-time augmentation detects newly resolved observable
                // references. The plan already decided the unobservable leaf
                // half, and keys it marked changed were skipped above.
                let changed = key_should_enter_patch(
                    key,
                    Some(schema),
                    LeafProvenance::DecidedByPlan { key_changed: false },
                    AttrComparison {
                        from: from.attributes.get(key),
                        to: comparison_value.as_ref(),
                        saved: None,
                        type_info,
                    },
                    &comparison,
                );
                if changed {
                    effective_changed.push(key.clone());
                }
            }
            let request =
                UpdateRequest::checked(from.clone(), &effective_changed, &resolved_to, schema);
            let accepted_request =
                AcceptedRequest::from_update_request(&request, resolved_desired_attributes.clone());
            match provider.update(id, identifier, request).await {
                Ok(outcome) => {
                    let diagnostic = match &outcome {
                        UpdateOutcome::Success { .. } => None,
                        UpdateOutcome::PartialSuccess { diagnostic, .. } => {
                            Some(diagnostic.clone())
                        }
                    };
                    let state = outcome.into_state_for_writeback();
                    if let Some(diagnostic) = diagnostic {
                        BasicEffectResult::PartialSuccess {
                            completed: CompletedEffect {
                                state: Box::new(state),
                                request: accepted_request,
                            },
                            resource_id: id.clone(),
                            diagnostic,
                            resolved_attrs: Some(resolved_desired_attributes),
                            binding: to.binding.clone(),
                        }
                    } else {
                        BasicEffectResult::Success {
                            completed: CompletedEffect {
                                state: Box::new(state),
                                request: accepted_request,
                            },
                            resource_id: id.clone(),
                            resolved_attrs: Some(resolved_desired_attributes),
                            binding: to.binding.clone(),
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    BasicEffectResult::Failure {
                        error: error_str,
                        refresh: Some((id.clone(), identifier.clone())),
                    }
                }
            }
        }
        BasicEffect::Delete {
            id,
            identifier,
            generation,
            directives,
            ..
        } => {
            match provider
                .delete(
                    id,
                    identifier,
                    DeleteRequest {
                        directives: directives.clone(),
                    },
                )
                .await
            {
                Ok(()) => BasicEffectResult::Deleted {
                    instance_key: DeletedInstanceKey::new(
                        id.clone(),
                        generation.clone(),
                        identifier.to_string(),
                    ),
                },
                Err(e) => {
                    let error_str = e.to_string();
                    BasicEffectResult::Failure {
                        error: error_str,
                        refresh: match generation {
                            EffectGeneration::Current => Some((id.clone(), identifier.clone())),
                            EffectGeneration::Deposed(_) => None,
                        },
                    }
                }
            }
        }
    };
    StartedBasicEffectResult::new(started_effect, started, outcome)
}

#[cfg(test)]
mod process_basic_result_tests {
    use super::*;
    use crate::executor::ExecutionResult;
    use crate::provider::PartialReadDiagnostic;
    use crate::resource::State;
    use std::collections::HashSet;

    fn empty_execution_result() -> ExecutionResult {
        ExecutionResult {
            success_count: 0,
            failure_count: 0,
            partial_count: 0,
            partial_diagnostics: Vec::new(),
            skip_count: 0,
            interrupted_count: 0,
            applied_states: HashMap::new(),
            runtime_synthesized_resources: Vec::new(),
            successfully_deleted: HashSet::new(),
            permanent_name_overrides: HashMap::new(),
            managed_current: HashMap::new(),
            data_source_states: HashMap::new(),
            bindings: ResolvedBindings::default(),
            failed_refreshes: HashSet::new(),
        }
    }

    #[test]
    fn published_non_data_source_state_overwrites_wait_identifier() {
        let managed_id =
            ResourceId::with_provider_identity("mock", "test.resource", "waited", None);
        let managed_identifier = crate::provider::AppliedIdentifier::new("waited-id").unwrap();
        let managed_state =
            State::existing(managed_id.clone(), HashMap::new()).with_identifier("waited-id");
        let data_source_id =
            ResourceId::with_provider_identity("mock", "test.lookup", "lookup", None);
        let data_source_state =
            State::existing(data_source_id.clone(), HashMap::new()).with_identifier("lookup-id");
        let (mut accumulator, wait_identifiers) =
            ExecutionAccumulator::new(empty_execution_result());

        accumulator.insert_published(
            managed_id.clone(),
            Applied::Imported(managed_state.clone()),
            &managed_state,
        );
        accumulator.insert_published(
            data_source_id.clone(),
            Applied::DataSource(data_source_state.clone()),
            &data_source_state,
        );

        assert_eq!(
            crate::executor::wait::resolve_wait_identifier(&wait_identifiers, &managed_id),
            Some(managed_identifier),
            "managed publication must update the wait identifier registry"
        );

        let ended_state = State::existing(managed_id.clone(), HashMap::new());
        accumulator.insert_published(managed_id.clone(), Applied::CreateEnded, &ended_state);
        assert_eq!(
            crate::executor::wait::resolve_wait_identifier(&wait_identifiers, &managed_id),
            None,
            "ended create publication must clear a stale wait identifier"
        );

        assert_eq!(
            crate::executor::wait::resolve_wait_identifier(&wait_identifiers, &data_source_id),
            None,
            "data-source publication must not update the managed wait identifier registry"
        );
    }

    #[test]
    fn partial_success_records_state_and_diagnostic() {
        let id = ResourceId::with_provider_identity("mock", "test.resource", "r1", None);
        let state = State::existing(id.clone(), HashMap::new()).with_identifier("mock-id");
        let diagnostic = PartialReadDiagnostic::new(
            "mock partial create".to_string(),
            vec!["computed".to_string()],
        )
        .expect("missing attributes are non-empty");
        let writeback_state = diagnostic.clone().into_state_for_writeback(state.clone());
        let (mut accumulator, _) = ExecutionAccumulator::new(empty_execution_result());
        let mut failed_indices = HashSet::new();
        let mut pending_refreshes = HashMap::new();
        let mut exec = ExecutionState {
            idx: 7,
            accumulator: &mut accumulator,
            failed_indices: &mut failed_indices,
            pending_refreshes: &mut pending_refreshes,
        };

        process_basic_outcome(
            BasicEffectResult::PartialSuccess {
                completed: CompletedEffect {
                    state: Box::new(writeback_state),
                    request: AcceptedRequest::create(),
                },
                resource_id: id.clone(),
                diagnostic: diagnostic.clone(),
                resolved_attrs: None::<SecretPreservingAttributes>,
                binding: None,
            },
            &mut exec,
        );

        let result = accumulator.result();
        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 0);
        assert_eq!(result.partial_count, 1);
        assert_eq!(result.partial_diagnostics, vec![(id.clone(), diagnostic)]);
        let applied = accumulator
            .applied_states()
            .get(&id)
            .expect("partial state must be recorded");
        let applied = match applied {
            Applied::Complete {
                state,
                request: _request,
            } => state,
            Applied::Observed(_created) => panic!("unexpected observed create"),
            Applied::CreateInFlight(_pending) => panic!("unexpected in-flight create"),
            Applied::CreateOutcomeUnknown(_unknown) => {
                panic!("unexpected unknown create outcome")
            }
            Applied::CreateEnded => panic!("unexpected ended create"),
            Applied::Imported(_state) => panic!("unexpected import"),
            Applied::DataSource(_state) => panic!("unexpected data source"),
            Applied::WaitSnapshot(_state) => panic!("unexpected wait snapshot"),
        };
        assert_eq!(applied.identifier, state.identifier);
        assert_eq!(applied.attributes, state.attributes);
        let marker = applied
            .partial_read
            .as_ref()
            .expect("partial result records a marker");
        assert_eq!(
            marker.pending(),
            &crate::resource::PendingReRead::incomplete("mock partial create")
        );
        assert!(marker.missing_attributes().contains("computed"));
    }
}

#[cfg(test)]
mod assert_fully_resolved_tests {
    //! Unit-level coverage of the apply-time fail-fast contract added
    //! for carina#3032 (the "did not resolve at apply" matrix) and
    //! the kind-aware diagnostic added for carina#3334 (upstream_state
    //! binding gets a remediation that is actually actionable, not
    //! `wait`). The end-to-end executor variant lives at
    //! `executor::tests::test_chained_index_then_field_unresolved_at_apply_fails_with_clear_error`;
    //! these tests cover the deferred-variant matrix without going
    //! through the executor + mock provider scaffolding.
    use super::*;
    use crate::binding_index::{BindingValueSource, ResolvedBindings};
    use crate::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, PathSegment, Subscript,
        UnknownReason, Value,
    };

    fn dvo_chained_ref() -> Value {
        let path = AccessPath::with_segments(
            "cert",
            "domain_validation_options",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "resource_record_value".to_string(),
                },
            ],
        );
        Value::Deferred(DeferredValue::ResourceRef { path })
    }

    fn bindings_with(name: &str, source: BindingValueSource) -> ResolvedBindings {
        let mut b = ResolvedBindings::default();
        b.set(name, HashMap::new(), source);
        b
    }

    fn empty_bindings() -> ResolvedBindings {
        ResolvedBindings::default()
    }

    #[test]
    fn concrete_value_is_fully_resolved() {
        assert!(
            assert_fully_resolved(
                &Value::Concrete(ConcreteValue::String("ok".into())),
                "name",
                &empty_bindings(),
            )
            .is_ok()
        );
    }

    #[test]
    fn unknown_is_rejected_at_apply_seam() {
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
        let err = assert_fully_resolved(&v, "name", &empty_bindings())
            .expect_err("Unknown must be rejected at apply seam");
        assert!(err.contains("value is not yet known"), "got: {err}");
    }

    #[test]
    fn saved_plan_secret_placeholder_is_rejected_at_apply_seam() {
        let v = Value::Deferred(DeferredValue::RedactedSecret {
            hash: crate::resource::RedactedSecretHash::generate(b"placeholder"),
        });
        let err = assert_fully_resolved(&v, "password", &empty_bindings())
            .expect_err("saved-plan placeholder must be rejected before provider dispatch");
        assert!(err.contains("not rehydrated"), "got: {err}");
        assert!(err.contains("new saved plan"), "got: {err}");
    }

    #[test]
    fn direct_chained_resource_ref_is_rejected_with_path_and_wait_hint() {
        let err = assert_fully_resolved(&dvo_chained_ref(), "name", &empty_bindings())
            .expect_err("direct ResourceRef must be rejected at apply seam");
        assert!(
            err.contains("cert.domain_validation_options[0].resource_record_value"),
            "error must name the unresolved path; got: {err}",
        );
        assert!(
            err.contains("wait"),
            "error must point at `wait` as the workaround; got: {err}",
        );
    }

    #[test]
    fn list_wrapped_chained_ref_is_rejected() {
        // The exact carina#3032 failing form: `resource_records =
        // [cert.dvo[0].rrv]`. The list literal must be walked.
        let v = Value::Concrete(ConcreteValue::List(vec![dvo_chained_ref()]));
        let err = assert_fully_resolved(&v, "resource_records", &empty_bindings())
            .expect_err("list-wrapped ResourceRef must be rejected");
        assert!(err.contains("resource_record_value"), "got: {err}");
    }

    #[test]
    fn map_wrapped_chained_ref_is_rejected() {
        let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        m.insert("nested".to_string(), dvo_chained_ref());
        let err = assert_fully_resolved(
            &Value::Concrete(ConcreteValue::Map(m)),
            "tags",
            &empty_bindings(),
        )
        .expect_err("map-wrapped ResourceRef must be rejected");
        assert!(err.contains("resource_record_value"), "got: {err}");
    }

    #[test]
    fn secret_wrapped_ref_is_peeled_and_rejected() {
        // `Secret(ResourceRef)` would otherwise sneak past — the
        // secret tag would survive but the ref would still be
        // unresolved at the WASM boundary.
        let v = Value::Deferred(DeferredValue::Secret(Box::new(dvo_chained_ref())));
        let err = assert_fully_resolved(&v, "password", &empty_bindings())
            .expect_err("Secret wrapping does not exempt unresolved refs");
        assert!(err.contains("resource_record_value"), "got: {err}");
    }

    #[test]
    fn binding_ref_is_rejected() {
        let v = Value::Deferred(DeferredValue::BindingRef {
            binding: "vpc".to_string(),
        });
        let err = assert_fully_resolved(&v, "vpc_id", &empty_bindings())
            .expect_err("bare BindingRef must be rejected");
        assert!(err.contains("vpc"), "got: {err}");
        assert!(err.contains("wait"), "got: {err}");
    }

    #[test]
    fn interpolation_with_unresolved_part_is_rejected() {
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(dvo_chained_ref()),
        ]));
        let err = assert_fully_resolved(&v, "name", &empty_bindings())
            .expect_err("Interpolation that did not collapse must be rejected");
        assert!(err.contains("interpolation"), "got: {err}");
    }

    #[test]
    fn function_call_with_unresolved_args_is_rejected() {
        let v = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::concat,
            args: vec![dvo_chained_ref().into()],
            source_line: (0).into(),
        });
        let err = assert_fully_resolved(&v, "name", &empty_bindings())
            .expect_err("FunctionCall that did not evaluate must be rejected");
        assert!(err.contains("concat"), "got: {err}");
    }

    #[test]
    fn computed_list_out_of_range_surfaces_range_error_before_provider_boundary() {
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
                    Value::resource_ref("q", "identifier", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 5 },
            },
            4,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();

        let error = resolve_ref_value(&value, &bindings)
            .and_then(|resolved| assert_fully_resolved(&resolved, "name", &bindings))
            .expect_err("the apply path must reject the final computed list access");

        assert!(
            error.contains("index 5 out of range for list of length 3"),
            "unexpected apply error: {error}"
        );
        assert!(
            !error.contains("did not fully resolve") && !error.contains("wait"),
            "a concrete range failure must not use unresolved/wait wording: {error}"
        );
    }

    // -------------------------------------------------------------
    // carina#3334: kind-aware remediation for `upstream_state`
    // bindings. The pre-fix message proposed a `wait` block, which
    // is structurally inapplicable to upstream_state reads — `wait`
    // synchronizes on managed-resource lifecycles in *this* config.
    // -------------------------------------------------------------

    /// `infra.cloudfront_distribution_id` where `infra` is an
    /// `upstream_state` binding whose owning stack has not been
    /// applied yet (the export is absent from the upstream's saved
    /// state). The diagnostic must point at "apply the upstream
    /// stack first", not at `wait`.
    #[test]
    fn resource_ref_to_upstream_state_binding_does_not_suggest_wait() {
        let path = AccessPath::with_segments("infra", "cloudfront_distribution_id", vec![]);
        let v = Value::Deferred(DeferredValue::ResourceRef { path });
        let bindings = bindings_with("infra", BindingValueSource::Upstream);

        let err = assert_fully_resolved(&v, "policy_document", &bindings)
            .expect_err("ResourceRef against unpublished upstream_state must be rejected");

        assert!(
            !err.contains("Add a `wait` block")
                && !err.contains("add a `wait` block on the upstream binding"),
            "upstream_state remediation must not suggest a `wait` block; got: {err}",
        );
        assert!(
            err.contains("upstream_state") && err.contains("Apply the upstream stack first"),
            "upstream_state remediation must name `upstream_state` and tell the user to apply the upstream stack first; got: {err}",
        );
        assert!(
            err.contains("infra"),
            "error must name the upstream binding; got: {err}",
        );
    }

    /// The exact failing form from carina#3334's apply trace: an
    /// interpolation that did not collapse, whose unresolved part
    /// reaches an upstream_state binding through an argument /
    /// `let` chain.
    #[test]
    fn interpolation_into_upstream_state_binding_does_not_suggest_wait() {
        let path = AccessPath::with_segments("infra", "cloudfront_distribution_id", vec![]);
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("arn:aws:cloudfront::123:distribution/".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef { path })),
        ]));
        let bindings = bindings_with("infra", BindingValueSource::Upstream);

        let err = assert_fully_resolved(&v, "policy_document", &bindings)
            .expect_err("Interpolation containing upstream_state ref must be rejected");

        assert!(
            !err.contains("Add a `wait` block")
                && !err.contains("add a `wait` block on the upstream"),
            "interpolation-with-upstream_state must not suggest `wait`; got: {err}",
        );
        assert!(
            err.contains("upstream_state") && err.contains("Apply the upstream stack first"),
            "upstream_state remediation expected; got: {err}",
        );
    }

    /// `BindingRef` (bare alias, not a path) against an
    /// upstream_state binding must also get the upstream-aware
    /// remediation.
    #[test]
    fn binding_ref_to_upstream_state_binding_does_not_suggest_wait() {
        let v = Value::Deferred(DeferredValue::BindingRef {
            binding: "infra".to_string(),
        });
        let bindings = bindings_with("infra", BindingValueSource::Upstream);

        let err = assert_fully_resolved(&v, "policy_document", &bindings)
            .expect_err("BindingRef against upstream_state binding must be rejected");

        assert!(
            !err.contains("Add a `wait` block")
                && !err.contains("add a `wait` block on the upstream"),
            "upstream_state BindingRef must not suggest `wait`; got: {err}",
        );
        assert!(
            err.contains("upstream_state"),
            "remediation must name `upstream_state`; got: {err}",
        );
    }

    /// An interpolation whose parts reference both a local
    /// managed-resource binding and an `upstream_state` binding must
    /// surface the upstream-state remediation: that is the binding
    /// the operator must address outside the current configuration,
    /// and `wait` (the local-binding remediation) cannot fix it.
    #[test]
    fn interpolation_with_mixed_bindings_prefers_upstream_state_remediation() {
        let local_path = AccessPath::with_segments("cert", "domain_validation_options", vec![]);
        let upstream_path =
            AccessPath::with_segments("infra", "cloudfront_distribution_id", vec![]);
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: local_path,
            })),
            InterpolationPart::Literal("-".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: upstream_path,
            })),
        ]));

        let mut bindings = ResolvedBindings::default();
        bindings.set("cert", HashMap::new(), BindingValueSource::Local);
        bindings.set("infra", HashMap::new(), BindingValueSource::Upstream);

        let err = assert_fully_resolved(&v, "policy_document", &bindings)
            .expect_err("Interpolation with unresolved upstream ref must be rejected");

        assert!(
            err.contains("upstream_state") && err.contains("Apply the upstream stack first"),
            "mixed-binding remediation must point at upstream_state; got: {err}",
        );
        assert!(
            err.contains("infra"),
            "remediation must name the upstream binding (`infra`); got: {err}",
        );
    }

    /// Sanity check: when the referenced binding is a *local*
    /// managed-resource binding (the original carina#3032 scenario),
    /// the existing `wait`-block remediation must still appear —
    /// the kind-aware branch only kicks in for `Upstream`.
    #[test]
    fn local_binding_keeps_existing_wait_block_hint() {
        let path = AccessPath::with_segments("cert", "domain_validation_options", vec![]);
        let v = Value::Deferred(DeferredValue::ResourceRef { path });
        let bindings = bindings_with("cert", BindingValueSource::Local);

        let err = assert_fully_resolved(&v, "resource_records", &bindings)
            .expect_err("Local ResourceRef must still be rejected with wait hint");

        assert!(
            err.contains("wait"),
            "local-binding error must still mention `wait`; got: {err}",
        );
        assert!(
            !err.contains("upstream_state"),
            "local-binding error must not invoke upstream_state wording; got: {err}",
        );
    }
}
