//! Single-effect execution: Create, Update, Delete dispatch, resource resolution,
//! Secret unwrapping, and binding map updates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::effect::Effect;
use crate::provider::Provider;
use crate::resolver::resolve_ref_value;
use crate::resource::{Expr, Resource, ResourceId, State, Value};

use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};

/// Result of executing a basic effect (Create, Update, or Delete).
///
/// This is the shared result type used by `execute_basic_effect` to avoid
/// duplicating effect dispatch logic across sequential and phased paths.
pub(super) enum BasicEffectResult {
    Success {
        state: Option<State>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
        binding: Option<String>,
    },
    Failure {
        binding: Option<String>,
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
    pub(super) success_count: &'a mut usize,
    pub(super) failure_count: &'a mut usize,
    pub(super) applied_states: &'a mut HashMap<ResourceId, State>,
    pub(super) failed_bindings: &'a mut std::collections::HashSet<String>,
    pub(super) successfully_deleted: &'a mut std::collections::HashSet<ResourceId>,
    pub(super) pending_refreshes: &'a mut HashMap<ResourceId, String>,
    pub(super) binding_map: &'a mut HashMap<String, HashMap<String, Value>>,
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
    refreshes.sort_by(|(left_id, _), (right_id, _)| left_id.to_string().cmp(&right_id.to_string()));
    let mut failed_refreshes = std::collections::HashSet::new();

    for (id, identifier) in refreshes {
        match provider.read(id, Some(identifier)).await {
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

/// Resolve a resource's attributes using the current binding map.
/// Secret values are unwrapped so the provider receives the plain inner value.
pub(super) fn resolve_resource(
    resource: &Resource,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Result<Resource, String> {
    let mut resolved = resource.clone();
    for (key, expr) in &resource.attributes {
        let resolved_value = resolve_ref_value(expr, binding_map)?;
        resolved
            .attributes
            .insert(key.clone(), Expr(unwrap_secret(resolved_value)));
    }
    Ok(resolved)
}

/// Resolve a resource, preferring unresolved source for re-resolution.
/// Secret values are unwrapped so the provider receives the plain inner value.
pub(super) fn resolve_resource_with_source(
    target: &Resource,
    source: &Resource,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Result<Resource, String> {
    let mut resolved = target.clone();
    for (key, expr) in &source.attributes {
        let resolved_value = resolve_ref_value(expr, binding_map)?;
        resolved
            .attributes
            .insert(key.clone(), Expr(unwrap_secret(resolved_value)));
    }
    Ok(resolved)
}

/// Recursively unwrap `Value::Secret(inner)` to just the inner value.
/// This ensures the provider never sees the Secret wrapper.
fn unwrap_secret(value: Value) -> Value {
    match value {
        Value::Secret(inner) => unwrap_secret(*inner),
        Value::List(items) => Value::List(items.into_iter().map(unwrap_secret).collect()),
        Value::Map(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, unwrap_secret(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Update the binding map with a newly created/updated resource's state.
pub(super) fn update_binding_map(
    binding_map: &mut HashMap<String, HashMap<String, Value>>,
    resource_attrs: &HashMap<String, Value>,
    binding: Option<&str>,
    state: &State,
) {
    if let Some(binding_name) = binding {
        let mut attrs = resource_attrs.clone();
        for (k, v) in &state.attributes {
            attrs.insert(k.clone(), v.clone());
        }
        binding_map.insert(binding_name.to_string(), attrs);
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
                    update_binding_map(exec.binding_map, attrs, binding.as_deref(), &s);
                }
                exec.applied_states.insert(resource_id, s);
            }
        }
        BasicEffectResult::Failure {
            binding, refresh, ..
        } => {
            *exec.failure_count += 1;
            if let Some(binding) = binding {
                exec.failed_bindings.insert(binding);
            }
            if let Some((id, identifier)) = &refresh {
                queue_state_refresh(exec.pending_refreshes, id, Some(identifier.as_str()));
            }
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
        .filter(|e| !matches!(e, Effect::Read { .. }) && !e.is_state_operation())
        .count()
}

/// Execute a single Create, Update, or Delete effect.
///
/// This helper encapsulates the shared dispatch logic: increment progress counter,
/// record start time, emit EffectStarted, resolve resource, call provider, and
/// emit EffectSucceeded/EffectFailed. Returns a `BasicEffectResult` that callers
/// map to their path-specific result types.
///
/// Panics if called with a Replace, Read, Import, Remove, or Move effect.
pub(super) async fn execute_basic_effect<'a>(
    effect: &'a Effect,
    provider: &'a dyn Provider,
    binding_map: &'a HashMap<String, HashMap<String, Value>>,
    unresolved: &'a HashMap<ResourceId, Resource>,
    completed: &'a AtomicUsize,
    total: usize,
    observer: &'a dyn ExecutionObserver,
) -> BasicEffectResult {
    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();
    let progress = ProgressInfo {
        completed: c,
        total,
    };
    observer.on_event(&ExecutionEvent::EffectStarted { effect });

    match effect {
        Effect::Create(resource) => {
            let resolved = match resolve_resource(resource, binding_map) {
                Ok(r) => r,
                Err(e) => {
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &e,
                        duration: started.elapsed(),
                        progress,
                    });
                    return BasicEffectResult::Failure {
                        binding: effect.binding_name(),
                        refresh: None,
                    };
                }
            };
            match provider.create(&resolved).await {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect,
                        state: Some(&state),
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Success {
                        state: Some(state),
                        resource_id: resource.id.clone(),
                        resolved_attrs: Some(resolved.resolved_attributes()),
                        binding: resource.binding.clone(),
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
                        binding: effect.binding_name(),
                        refresh: None,
                    }
                }
            }
        }
        Effect::Update { id, from, to, .. } => {
            let resolve_source = unresolved.get(id).unwrap_or(to);
            let resolved_to = match resolve_resource_with_source(to, resolve_source, binding_map) {
                Ok(r) => r,
                Err(e) => {
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &e,
                        duration: started.elapsed(),
                        progress,
                    });
                    return BasicEffectResult::Failure {
                        binding: effect.binding_name(),
                        refresh: None,
                    };
                }
            };
            let identifier = from.identifier.as_deref().unwrap_or("");
            match provider.update(id, identifier, from, &resolved_to).await {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect,
                        state: Some(&state),
                        duration: started.elapsed(),
                        progress,
                    });
                    BasicEffectResult::Success {
                        state: Some(state),
                        resource_id: id.clone(),
                        resolved_attrs: Some(resolved_to.resolved_attributes()),
                        binding: to.binding.clone(),
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
                        binding: effect.binding_name(),
                        refresh: Some((id.clone(), identifier.to_string())),
                    }
                }
            }
        }
        Effect::Delete {
            id,
            identifier,
            lifecycle,
            ..
        } => match provider.delete(id, identifier, lifecycle).await {
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
                    binding: None,
                    refresh: Some((id.clone(), identifier.clone())),
                }
            }
        },
        _ => unreachable!("execute_basic_effect called with non-basic effect"),
    }
}
