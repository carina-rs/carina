//! Single-effect execution: Create, Update, Delete dispatch, resource resolution,
//! Secret unwrapping, and post-apply binding updates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::binding_index::ResolvedBindings;
use crate::differ::{
    AttrComparison, TypedAttr, key_should_enter_patch, secret_grafted_comparison_view,
};
use crate::effect::{BasicEffect, Effect};
use crate::executor::UnresolvedResource;
use crate::executor::normalized::{NormalizedResource, apply_desired_normalization};
use crate::parser::ProviderConfig;
use crate::provider::{
    CreateRequest, DeleteRequest, PartialReadDiagnostic, Provider, ProviderNormalizer, ReadRequest,
    UpdateOutcome, UpdateRequest, build_update_patch,
};
use crate::resolver::resolve_ref_value;
use crate::resource::{
    ConcreteValue, DeferredValue, ResolvedResource, Resource, ResourceId, State, Value,
};
use crate::value::{SecretHashContext, SerializationContext, SerializationError};

use super::wait::AppliedStates;
use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};

/// Private capability token for constructing [`ResolvedResource`].
/// Only this module can create a value, so the checked constructor in
/// `resource` cannot be used by sibling modules as a convention-only
/// escape hatch.
pub(crate) struct ResolvedResourceToken(());

/// Result of executing a basic effect (Create, Update, or Delete).
///
/// This is the shared result type used by `execute_basic_effect` to avoid
/// duplicating effect dispatch logic across sequential and phased paths.
///
/// State payloads are boxed so the transient result enum stays small without
/// weakening the exhaustiveness of the success/partial/failure/delete cases.
pub(super) enum BasicEffectResult {
    Success {
        state: Option<Box<State>>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
        binding: Option<String>,
    },
    PartialSuccess {
        state: Box<State>,
        resource_id: ResourceId,
        diagnostic: PartialReadDiagnostic,
        resolved_attrs: Option<HashMap<String, Value>>,
        binding: Option<String>,
    },
    Failure {
        refresh: Option<(ResourceId, String)>,
    },
    Deleted {
        resource_id: ResourceId,
    },
}

/// Mutable execution state shared across effect processing.
///
/// Groups the counters, result maps, and binding state that are threaded
/// through every call to `process_basic_result`, reducing parameter count.
pub(super) struct ExecutionState<'a> {
    pub(super) idx: usize,
    pub(super) success_count: &'a mut usize,
    pub(super) failure_count: &'a mut usize,
    pub(super) partial_count: &'a mut usize,
    pub(super) partial_diagnostics: &'a mut Vec<(ResourceId, PartialReadDiagnostic)>,
    pub(super) applied_states: &'a mut AppliedStates,
    pub(super) failed_indices: &'a mut std::collections::HashSet<usize>,
    pub(super) successfully_deleted: &'a mut std::collections::HashSet<ResourceId>,
    pub(super) pending_refreshes: &'a mut HashMap<ResourceId, String>,
    pub(super) bindings: &'a mut ResolvedBindings,
}

/// Queue a state refresh for a resource after a failed operation.
pub(super) fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
    }
}

/// Refresh states for resources whose operations failed.
pub(super) async fn refresh_pending_states(
    provider: &dyn Provider,
    current_states: &mut HashMap<ResourceId, State>,
    pending_refreshes: &HashMap<ResourceId, String>,
    observer: &dyn ExecutionObserver,
) -> std::collections::HashSet<ResourceId> {
    if pending_refreshes.is_empty() {
        return std::collections::HashSet::new();
    }

    observer.on_event(&ExecutionEvent::RefreshStarted);

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by_key(|(left_id, _)| left_id.to_string());
    let mut failed_refreshes = std::collections::HashSet::new();

    for (id, identifier) in refreshes {
        match provider
            .read(id, Some(identifier.as_str()), ReadRequest)
            .await
        {
            Ok(state) => {
                observer.on_event(&ExecutionEvent::RefreshSucceeded { id });
                current_states.insert(id.clone(), state);
            }
            Err(error) => {
                let error_str = error.to_string();
                observer.on_event(&ExecutionEvent::RefreshFailed {
                    id,
                    error: &error_str,
                });
                failed_refreshes.insert(id.clone());
            }
        }
    }

    failed_refreshes
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
pub(super) async fn resolve_resource(
    resource: &Resource,
    bindings: &ResolvedBindings,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<ResolvedResource, String> {
    let mut resolved = resource.clone();
    for (key, expr) in &resource.attributes {
        let resolved_value = unwrap_secret(resolve_ref_value(expr, bindings)?);
        assert_fully_resolved(&resolved_value, key, bindings)?;
        resolved.attributes.insert(key.clone(), resolved_value);
    }
    let normalized = apply_desired_normalization(
        resolved,
        pipeline.provider_configs,
        pipeline.normalizer,
        pipeline.factories,
        pipeline.schemas,
    )
    .await;
    resolved_normalized_resource(normalized).map_err(|err| err.to_string())
}

/// Resolve a resource, preferring unresolved source for re-resolution.
/// Secret values are unwrapped so the provider receives the plain inner value.
///
/// See [`resolve_resource`] for the fail-fast contract on
/// still-deferred values.
pub(super) async fn resolve_resource_with_source(
    target: &Resource,
    source: &Resource,
    bindings: &ResolvedBindings,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<ResolvedResource, String> {
    let mut resolved = target.clone();
    for (key, expr) in &source.attributes {
        let resolved_value = unwrap_secret(resolve_ref_value(expr, bindings)?);
        assert_fully_resolved(&resolved_value, key, bindings)?;
        resolved.attributes.insert(key.clone(), resolved_value);
    }
    let normalized = apply_desired_normalization(
        resolved,
        pipeline.provider_configs,
        pipeline.normalizer,
        pipeline.factories,
        pipeline.schemas,
    )
    .await;
    resolved_normalized_resource(normalized).map_err(|err| err.to_string())
}

#[cfg(test)]
pub(super) fn resolved_resource(
    resource: Resource,
) -> Result<ResolvedResource, SerializationError> {
    ResolvedResource::new(resource, ResolvedResourceToken(()))
}

pub(super) fn resolved_normalized_resource(
    resource: NormalizedResource,
) -> Result<ResolvedResource, SerializationError> {
    resource.into_resolved_resource(ResolvedResourceToken(()))
}

/// The full plan-time normalization pipeline, threaded into the apply
/// executor so reference re-resolution cannot undo it.
///
/// Bundled into one struct (rather than three separate args) so the
/// resolve helpers and `BasicEffectCtx` carry a single field.
pub(super) struct RenormalizePipeline<'a> {
    pub(super) normalizer: &'a dyn ProviderNormalizer,
    pub(super) provider_configs: &'a [ProviderConfig],
    pub(super) factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    pub(super) schemas: &'a crate::schema::SchemaRegistry,
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
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            let leaf = pick_unresolved_binding_for_diagnostic(value, bindings);
            Err(unresolved_binding_message(
                attribute_key,
                UnresolvedSite::Interpolation,
                leaf.unwrap_or(""),
                bindings,
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall { name, .. }) => {
            let leaf = pick_unresolved_binding_for_diagnostic(value, bindings);
            Err(unresolved_binding_message(
                attribute_key,
                UnresolvedSite::FunctionCall { name: name.clone() },
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
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for p in parts {
                if let InterpolationPart::Expr(v) = p {
                    collect_unresolved_bindings(v, out);
                }
            }
        }
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            for a in args {
                collect_unresolved_bindings(a, out);
            }
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

/// Recursively unwrap `Value::Deferred(DeferredValue::Secret(inner))` to just the inner value.
/// This ensures the provider never sees the Secret wrapper.
fn unwrap_secret(value: Value) -> Value {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => unwrap_secret(*inner),
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items.into_iter().map(unwrap_secret).collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.into_iter()
                .map(|(k, v)| (k, unwrap_secret(v)))
                .collect(),
        )),
        other => other,
    }
}

/// Process a `BasicEffectResult` by updating shared execution state.
///
/// This helper is used by both sequential and phased execution paths to avoid
/// duplicating the result-processing logic for Create/Update/Delete effects.
pub(super) fn process_basic_result(result: BasicEffectResult, exec: &mut ExecutionState<'_>) {
    match result {
        BasicEffectResult::Success {
            state: effect_state,
            resource_id,
            resolved_attrs,
            binding,
        } => {
            *exec.success_count += 1;
            if let Some(s) = effect_state {
                if let Some(attrs) = &resolved_attrs {
                    exec.bindings.record_applied(binding.as_deref(), attrs, &s);
                }
                exec.applied_states.insert(resource_id, *s);
            }
        }
        BasicEffectResult::Failure { refresh } => {
            *exec.failure_count += 1;
            exec.failed_indices.insert(exec.idx);
            if let Some((id, identifier)) = &refresh {
                queue_state_refresh(exec.pending_refreshes, id, Some(identifier.as_str()));
            }
        }
        BasicEffectResult::PartialSuccess {
            state,
            resource_id,
            diagnostic,
            resolved_attrs,
            binding,
        } => {
            *exec.partial_count += 1;
            if let Some(attrs) = &resolved_attrs {
                exec.bindings
                    .record_applied(binding.as_deref(), attrs, &state);
            }
            exec.applied_states.insert(resource_id.clone(), *state);
            exec.partial_diagnostics.push((resource_id, diagnostic));
        }
        BasicEffectResult::Deleted { resource_id, .. } => {
            *exec.success_count += 1;
            exec.successfully_deleted.insert(resource_id);
        }
    }
}

/// Count the number of actionable effects (excluding Read and state operations).
pub(super) fn count_actionable_effects(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| {
            !matches!(e, Effect::Read { .. }) && !e.is_state_operation() && !e.is_scheduler_meta()
        })
        .count()
}

/// Resolution + dispatch context for a basic effect, bundled to keep
/// `execute_basic_effect`'s arity in check (clippy `too_many_arguments`)
///.
pub(super) struct BasicEffectCtx<'a> {
    pub(super) provider: &'a dyn Provider,
    pub(super) bindings: &'a ResolvedBindings,
    pub(super) unresolved: &'a HashMap<ResourceId, UnresolvedResource>,
    pub(super) pipeline: &'a RenormalizePipeline<'a>,
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
/// Returns a `BasicEffectResult` that callers map to their path-specific
/// result types.
pub(super) async fn execute_basic_effect<'a>(
    basic: BasicEffect<'a>,
    ctx: &BasicEffectCtx<'a>,
    observer: &'a dyn ExecutionObserver,
) -> BasicEffectResult {
    let provider = ctx.provider;
    let bindings = ctx.bindings;
    let unresolved = ctx.unresolved;
    let pipeline = ctx.pipeline;
    let completed = ctx.completed;
    let total = ctx.total;
    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();
    let progress = ProgressInfo {
        completed: c,
        total,
    };
    let effect = basic.as_effect();
    observer.on_event(&ExecutionEvent::EffectStarted { effect });

    match basic {
        BasicEffect::Create { resource, .. } => {
            let resolved = match resolve_resource(resource, bindings, pipeline).await {
                Ok(r) => r,
                Err(e) => {
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &e,
                        duration: started.elapsed(),
                        progress,
                    });
                    return BasicEffectResult::Failure { refresh: None };
                }
            };
            let resolved_attrs = resolved.as_resource().resolved_attributes();
            match provider
                .create(&resource.id, CreateRequest { resource: resolved })
                .await
            {
                Ok(outcome) => {
                    let diagnostic = outcome.diagnostic().cloned();
                    let state = outcome.into_state_for_writeback();
                    if let Some(diagnostic) = diagnostic {
                        observer.on_event(&ExecutionEvent::EffectPartiallySucceeded {
                            effect,
                            state: &state,
                            diagnostic: &diagnostic,
                            duration: started.elapsed(),
                            progress,
                        });
                        BasicEffectResult::PartialSuccess {
                            state: Box::new(state),
                            resource_id: resource.id.clone(),
                            diagnostic,
                            resolved_attrs: Some(resolved_attrs),
                            binding: resource.binding.clone(),
                        }
                    } else {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                            progress,
                        });
                        BasicEffectResult::Success {
                            state: Some(Box::new(state)),
                            resource_id: resource.id.clone(),
                            resolved_attrs: Some(resolved_attrs),
                            binding: resource.binding.clone(),
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Failure { refresh: None }
                }
            }
        }
        BasicEffect::Update {
            id,
            from,
            to,
            changed_attributes,
            ..
        } => {
            let from = from.clone();
            let resolve_source = unresolved
                .get(id)
                .map_or(to, UnresolvedResource::as_resource);
            let resolved_to =
                match resolve_resource_with_source(to, resolve_source, bindings, pipeline).await {
                    Ok(r) => r,
                    Err(e) => {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &e,
                            duration: started.elapsed(),
                            progress,
                        });
                        return BasicEffectResult::Failure { refresh: None };
                    }
                };
            let identifier = from.identifier.as_deref().unwrap_or("");
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
            let schema = pipeline.schemas.get_for(resolved_resource);
            for (key, new_value) in &resolved_resource.attributes {
                if effective_changed.iter().any(|k| k == key) {
                    continue;
                }
                let type_info = schema.and_then(|s| {
                    s.attributes.get(key).map(|attr| TypedAttr {
                        attr_type: &attr.attr_type,
                        defs: &s.defs,
                    })
                });
                let secret_ctx = Some(SecretHashContext::new(
                    id.display_type(),
                    id.name_str(),
                    key,
                ));
                let Some(comparison_value) =
                    secret_grafted_comparison_view(new_value, resolve_source.attributes.get(key))
                else {
                    continue;
                };
                if key_should_enter_patch(
                    key,
                    schema,
                    AttrComparison {
                        from: from.attributes.get(key),
                        to: comparison_value.as_ref(),
                        saved: None,
                        type_info,
                        secret_ctx: secret_ctx.as_ref(),
                    },
                ) {
                    effective_changed.push(key.clone());
                }
            }
            let patch = build_update_patch(&effective_changed, &resolved_to, &from);
            let request = UpdateRequest {
                from: from.clone(),
                patch,
            };
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
                        observer.on_event(&ExecutionEvent::EffectPartiallySucceeded {
                            effect,
                            state: &state,
                            diagnostic: &diagnostic,
                            duration: started.elapsed(),
                            progress,
                        });
                        BasicEffectResult::PartialSuccess {
                            state: Box::new(state),
                            resource_id: id.clone(),
                            diagnostic,
                            resolved_attrs: Some(resolved_to.as_resource().resolved_attributes()),
                            binding: to.binding.clone(),
                        }
                    } else {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                            progress,
                        });
                        BasicEffectResult::Success {
                            state: Some(Box::new(state)),
                            resource_id: id.clone(),
                            resolved_attrs: Some(resolved_to.as_resource().resolved_attributes()),
                            binding: to.binding.clone(),
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Failure {
                        refresh: Some((id.clone(), identifier.to_string())),
                    }
                }
            }
        }
        BasicEffect::Delete {
            id,
            identifier,
            directives,
            ..
        } => match provider
            .delete(
                id,
                identifier,
                DeleteRequest {
                    directives: directives.clone(),
                },
            )
            .await
        {
            Ok(()) => {
                observer.on_event(&ExecutionEvent::EffectSucceeded {
                    effect,
                    state: None,
                    duration: started.elapsed(),
                    progress,
                });
                BasicEffectResult::Deleted {
                    resource_id: id.clone(),
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                observer.on_event(&ExecutionEvent::EffectFailed {
                    effect,
                    error: &error_str,
                    duration: started.elapsed(),
                    progress,
                });
                BasicEffectResult::Failure {
                    refresh: Some((id.clone(), identifier.to_string())),
                }
            }
        },
    }
}

#[cfg(test)]
mod process_basic_result_tests {
    use super::*;
    use crate::provider::PartialReadDiagnostic;
    use crate::resource::{State, Value};
    use std::collections::HashSet;

    #[test]
    fn partial_success_records_state_and_diagnostic() {
        let id = ResourceId::with_provider("mock", "test.resource", "r1", None);
        let state = State::existing(id.clone(), HashMap::new()).with_identifier("mock-id");
        let diagnostic = PartialReadDiagnostic::new(
            "mock partial create".to_string(),
            vec!["computed".to_string()],
        )
        .expect("missing attributes are non-empty");
        let writeback_state = diagnostic.clone().into_state_for_writeback(state.clone());
        let (mut applied_states, _) = AppliedStates::with_initial(&HashMap::new());
        let mut success_count = 0;
        let mut failure_count = 0;
        let mut partial_count = 0;
        let mut partial_diagnostics = Vec::new();
        let mut failed_indices = HashSet::new();
        let mut successfully_deleted = HashSet::new();
        let mut pending_refreshes = HashMap::new();
        let mut bindings = ResolvedBindings::default();
        let mut exec = ExecutionState {
            idx: 7,
            success_count: &mut success_count,
            failure_count: &mut failure_count,
            partial_count: &mut partial_count,
            partial_diagnostics: &mut partial_diagnostics,
            applied_states: &mut applied_states,
            failed_indices: &mut failed_indices,
            successfully_deleted: &mut successfully_deleted,
            pending_refreshes: &mut pending_refreshes,
            bindings: &mut bindings,
        };

        process_basic_result(
            BasicEffectResult::PartialSuccess {
                state: Box::new(writeback_state),
                resource_id: id.clone(),
                diagnostic: diagnostic.clone(),
                resolved_attrs: None::<HashMap<String, Value>>,
                binding: None,
            },
            &mut exec,
        );

        assert_eq!(success_count, 0);
        assert_eq!(failure_count, 0);
        assert_eq!(partial_count, 1);
        assert_eq!(partial_diagnostics, vec![(id.clone(), diagnostic)]);
        let states = applied_states.into_inner();
        let applied = states.get(&id).expect("partial state must be recorded");
        assert_eq!(applied.identifier, state.identifier);
        assert_eq!(applied.attributes, state.attributes);
        assert_eq!(
            applied.partial_read.as_ref().map(|marker| (
                marker.detail.as_str(),
                marker.missing_attributes.contains("computed")
            )),
            Some(("mock partial create", true))
        );
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
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(dvo_chained_ref()),
        ]));
        let err = assert_fully_resolved(&v, "name", &empty_bindings())
            .expect_err("Interpolation that did not collapse must be rejected");
        assert!(err.contains("interpolation"), "got: {err}");
    }

    #[test]
    fn function_call_with_unresolved_args_is_rejected() {
        let v = Value::Deferred(DeferredValue::FunctionCall {
            name: "concat".to_string(),
            args: vec![dvo_chained_ref()],
        });
        let err = assert_fully_resolved(&v, "name", &empty_bindings())
            .expect_err("FunctionCall that did not evaluate must be rejected");
        assert!(err.contains("concat"), "got: {err}");
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
            InterpolationPart::Literal("arn:aws:cloudfront::123:distribution/".to_string()),
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
            InterpolationPart::Literal("-".to_string()),
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
