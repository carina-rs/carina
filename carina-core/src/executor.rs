//! Plan executor - Executes a Plan by dispatching Effects to a Provider.
//!
//! This module contains the core execution logic extracted from the CLI apply command.
//! It uses an `ExecutionObserver` trait for UI separation, allowing the CLI to provide
//! colored progress output while keeping the execution logic testable.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::deps::find_failed_dependency;
use crate::effect::Effect;
use crate::plan::Plan;
use crate::provider::Provider;
use crate::resolver::resolve_ref_value;
use crate::resource::{Resource, ResourceId, State, Value};

/// Input data required to execute a plan.
pub struct ExecutionInput<'a> {
    pub plan: &'a Plan,
    pub unresolved_resources: &'a HashMap<ResourceId, Resource>,
    pub binding_map: HashMap<String, HashMap<String, Value>>,
    pub current_states: HashMap<ResourceId, State>,
}

/// Result of executing a plan's effects.
pub struct ExecutionResult {
    pub success_count: usize,
    pub failure_count: usize,
    pub skip_count: usize,
    pub applied_states: HashMap<ResourceId, State>,
    pub successfully_deleted: HashSet<ResourceId>,
    pub permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub current_states: HashMap<ResourceId, State>,
    pub failed_refreshes: HashSet<ResourceId>,
}

/// Events emitted during plan execution.
pub enum ExecutionEvent<'a> {
    EffectStarted {
        effect: &'a Effect,
    },
    EffectSucceeded {
        effect: &'a Effect,
        state: Option<&'a State>,
        duration: Duration,
    },
    EffectFailed {
        effect: &'a Effect,
        error: &'a str,
        duration: Duration,
    },
    EffectSkipped {
        effect: &'a Effect,
        reason: &'a str,
    },
    CascadeUpdateSucceeded {
        id: &'a ResourceId,
    },
    CascadeUpdateFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    RenameSucceeded {
        id: &'a ResourceId,
        from: &'a str,
        to: &'a str,
    },
    RenameFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    RefreshStarted,
    RefreshSucceeded {
        id: &'a ResourceId,
    },
    RefreshFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
}

/// Observer trait for UI separation during plan execution.
pub trait ExecutionObserver {
    fn on_event(&mut self, event: &ExecutionEvent);
}

/// Execute a plan by dispatching effects to a provider.
///
/// This function contains the core execution logic, including:
/// - Reference resolution via binding_map
/// - 3-phase Replace ordering for interdependent replaces
/// - Binding map updates after each effect
/// - Failure propagation (failed_bindings)
/// - Dependency skip
/// - Pending state refreshes
pub async fn execute_plan(
    provider: &dyn Provider,
    mut input: ExecutionInput<'_>,
    observer: &mut dyn ExecutionObserver,
) -> ExecutionResult {
    if has_interdependent_replaces(input.plan.effects()) {
        execute_effects_phased(provider, &mut input, observer).await
    } else {
        execute_effects_sequential(provider, &mut input, observer).await
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Check if the plan contains multiple Replace effects that depend on each other.
fn has_interdependent_replaces(effects: &[Effect]) -> bool {
    let replace_bindings = collect_replace_bindings(effects);
    if replace_bindings.is_empty() {
        return false;
    }

    for effect in effects {
        if let Effect::Replace { to, .. } = effect {
            let dep_bindings = extract_dependency_bindings(&to.attributes);
            for dep in &dep_bindings {
                if replace_bindings.contains(dep) {
                    return true;
                }
            }
        }
    }
    false
}

/// Collect binding names from all Replace effects.
fn collect_replace_bindings(effects: &[Effect]) -> HashSet<String> {
    let mut bindings = HashSet::new();
    for effect in effects {
        if let Effect::Replace { to, .. } = effect
            && let Some(Value::String(b)) = to.attributes.get("_binding")
        {
            bindings.insert(b.clone());
        }
    }
    bindings
}

/// Extract `_dependency_bindings` from attributes.
fn extract_dependency_bindings(attrs: &HashMap<String, Value>) -> Vec<String> {
    match attrs.get("_dependency_bindings") {
        Some(Value::List(list)) => list
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// Topologically sort Replace effects by dependency order.
/// Returns indices in forward dependency order (parents before dependents).
fn topological_sort_replaces(effects: &[Effect], replace_bindings: &HashSet<String>) -> Vec<usize> {
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    let mut replace_indices: Vec<usize> = Vec::new();

    for (idx, effect) in effects.iter().enumerate() {
        if let Effect::Replace { to, .. } = effect {
            replace_indices.push(idx);
            if let Some(Value::String(b)) = to.attributes.get("_binding") {
                binding_to_idx.insert(b.clone(), idx);
            }
        }
    }

    // Build adjacency: for each replace effect, find which other replace effects it depends on
    let mut deps: HashMap<usize, Vec<usize>> = HashMap::new();
    for &idx in &replace_indices {
        let effect = &effects[idx];
        if let Effect::Replace { to, .. } = effect {
            let dep_indices: Vec<usize> = extract_dependency_bindings(&to.attributes)
                .iter()
                .filter(|b| replace_bindings.contains(*b))
                .filter_map(|b| binding_to_idx.get(b))
                .copied()
                .collect();
            deps.insert(idx, dep_indices);
        }
    }

    // Kahn's algorithm for topological sort
    let mut in_degree: HashMap<usize, usize> = HashMap::new();
    for &idx in &replace_indices {
        in_degree.insert(idx, 0);
    }
    for (&idx, dep_list) in &deps {
        *in_degree.entry(idx).or_insert(0) += dep_list.len();
    }

    let mut queue: Vec<usize> = replace_indices
        .iter()
        .filter(|idx| *in_degree.get(idx).unwrap_or(&0) == 0)
        .copied()
        .collect();
    queue.sort();

    let mut sorted = Vec::new();
    while let Some(node) = queue.pop() {
        sorted.push(node);
        for (&idx, dep_list) in &deps {
            if dep_list.contains(&node) {
                let deg = in_degree.get_mut(&idx).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push(idx);
                    queue.sort();
                }
            }
        }
    }

    // If there are nodes not in sorted (cycle), append them in original order
    for &idx in &replace_indices {
        if !sorted.contains(&idx) {
            sorted.push(idx);
        }
    }

    sorted
}

/// Queue a state refresh for a resource after a failed operation.
fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
    }
}

/// Refresh states for resources whose operations failed.
async fn refresh_pending_states(
    provider: &dyn Provider,
    current_states: &mut HashMap<ResourceId, State>,
    pending_refreshes: &HashMap<ResourceId, String>,
    observer: &mut dyn ExecutionObserver,
) -> HashSet<ResourceId> {
    if pending_refreshes.is_empty() {
        return HashSet::new();
    }

    observer.on_event(&ExecutionEvent::RefreshStarted);

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by(|(left_id, _), (right_id, _)| left_id.to_string().cmp(&right_id.to_string()));
    let mut failed_refreshes = HashSet::new();

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
fn resolve_resource(
    resource: &Resource,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Resource {
    let mut resolved = resource.clone();
    for (key, value) in &resource.attributes {
        resolved
            .attributes
            .insert(key.clone(), resolve_ref_value(value, binding_map));
    }
    resolved
}

/// Resolve a resource, preferring unresolved source for re-resolution.
fn resolve_resource_with_source(
    target: &Resource,
    source: &Resource,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Resource {
    let mut resolved = target.clone();
    for (key, value) in &source.attributes {
        resolved
            .attributes
            .insert(key.clone(), resolve_ref_value(value, binding_map));
    }
    resolved
}

/// Update the binding map with a newly created/updated resource's state.
fn update_binding_map(
    binding_map: &mut HashMap<String, HashMap<String, Value>>,
    resource_attrs: &HashMap<String, Value>,
    state: &State,
) {
    if let Some(Value::String(binding_name)) = resource_attrs.get("_binding") {
        let mut attrs = resource_attrs.clone();
        for (k, v) in &state.attributes {
            attrs.insert(k.clone(), v.clone());
        }
        binding_map.insert(binding_name.clone(), attrs);
    }
}

// ---------------------------------------------------------------------------
// Effect execution: sequential path
// ---------------------------------------------------------------------------

/// Execute effects sequentially (no dependency reordering).
async fn execute_effects_sequential(
    provider: &dyn Provider,
    input: &mut ExecutionInput<'_>,
    observer: &mut dyn ExecutionObserver,
) -> ExecutionResult {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let mut permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();

    for effect in input.plan.effects() {
        // Check if any dependency has failed - skip this effect if so
        if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
            let reason = format!("dependency '{}' failed", failed_dep);
            observer.on_event(&ExecutionEvent::EffectSkipped {
                effect,
                reason: &reason,
            });
            skip_count += 1;
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
            continue;
        }

        let started = Instant::now();
        observer.on_event(&ExecutionEvent::EffectStarted { effect });

        match effect {
            Effect::Create(resource) => {
                let resolved = resolve_resource(resource, &input.binding_map);
                match provider.create(&resolved).await {
                    Ok(state) => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                        });
                        success_count += 1;
                        applied_states.insert(resource.id.clone(), state.clone());
                        update_binding_map(&mut input.binding_map, &resolved.attributes, &state);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &error_str,
                            duration: started.elapsed(),
                        });
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Update { id, from, to, .. } => {
                let resolve_source = input.unresolved_resources.get(id).unwrap_or(to);
                let resolved_to =
                    resolve_resource_with_source(to, resolve_source, &input.binding_map);
                let identifier = from.identifier.as_deref().unwrap_or("");
                match provider.update(id, identifier, from, &resolved_to).await {
                    Ok(state) => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                        });
                        success_count += 1;
                        applied_states.insert(id.clone(), state.clone());
                        update_binding_map(&mut input.binding_map, &resolved_to.attributes, &state);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &error_str,
                            duration: started.elapsed(),
                        });
                        failure_count += 1;
                        queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Replace {
                id,
                from,
                to,
                lifecycle,
                cascading_updates,
                temporary_name,
                ..
            } => {
                if lifecycle.create_before_destroy {
                    execute_cbd_replace_sequential(
                        provider,
                        effect,
                        id,
                        from,
                        to,
                        lifecycle,
                        cascading_updates,
                        temporary_name.as_ref(),
                        &mut input.binding_map,
                        &mut applied_states,
                        &mut failed_bindings,
                        &mut permanent_name_overrides,
                        &mut pending_refreshes,
                        &mut success_count,
                        &mut failure_count,
                        started,
                        observer,
                    )
                    .await;
                } else {
                    execute_dbd_replace(
                        provider,
                        effect,
                        id,
                        from,
                        to,
                        lifecycle,
                        &mut input.binding_map,
                        &mut applied_states,
                        &mut failed_bindings,
                        &mut pending_refreshes,
                        &mut success_count,
                        &mut failure_count,
                        started,
                        observer,
                    )
                    .await;
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
                    });
                    success_count += 1;
                    successfully_deleted.insert(id.clone());
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                    });
                    failure_count += 1;
                    queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                }
            },
            Effect::Read { .. } => {}
        }
    }

    let failed_refreshes = refresh_pending_states(
        provider,
        &mut input.current_states,
        &pending_refreshes,
        observer,
    )
    .await;

    ExecutionResult {
        success_count,
        failure_count,
        skip_count,
        applied_states,
        successfully_deleted,
        permanent_name_overrides,
        current_states: input.current_states.clone(),
        failed_refreshes,
    }
}

// ---------------------------------------------------------------------------
// CBD Replace (create-before-destroy) - sequential path
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn execute_cbd_replace_sequential(
    provider: &dyn Provider,
    effect: &Effect,
    id: &ResourceId,
    from: &State,
    to: &Resource,
    lifecycle: &crate::resource::LifecycleConfig,
    cascading_updates: &[crate::effect::CascadingUpdate],
    temporary_name: Option<&crate::effect::TemporaryName>,
    binding_map: &mut HashMap<String, HashMap<String, Value>>,
    applied_states: &mut HashMap<ResourceId, State>,
    failed_bindings: &mut HashSet<String>,
    permanent_name_overrides: &mut HashMap<ResourceId, HashMap<String, String>>,
    pending_refreshes: &mut HashMap<ResourceId, String>,
    success_count: &mut usize,
    failure_count: &mut usize,
    started: Instant,
    observer: &mut dyn ExecutionObserver,
) {
    let resolved = resolve_resource(to, binding_map);
    match provider.create(&resolved).await {
        Ok(state) => {
            update_binding_map(binding_map, &resolved.attributes, &state);

            // Execute cascading updates
            let mut cascade_failed = false;
            for cascade in cascading_updates {
                let resolved_to = resolve_resource(&cascade.to, binding_map);
                let cascade_identifier = cascade.from.identifier.as_deref().unwrap_or("");
                match provider
                    .update(&cascade.id, cascade_identifier, &cascade.from, &resolved_to)
                    .await
                {
                    Ok(cascade_state) => {
                        observer
                            .on_event(&ExecutionEvent::CascadeUpdateSucceeded { id: &cascade.id });
                        applied_states.insert(cascade.id.clone(), cascade_state.clone());
                        update_binding_map(binding_map, &resolved_to.attributes, &cascade_state);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::CascadeUpdateFailed {
                            id: &cascade.id,
                            error: &error_str,
                        });
                        queue_state_refresh(
                            pending_refreshes,
                            &cascade.id,
                            Some(cascade_identifier),
                        );
                        cascade_failed = true;
                        *failure_count += 1;
                        break;
                    }
                }
            }

            if cascade_failed {
                queue_state_refresh(pending_refreshes, &to.id, state.identifier.as_deref());
                if let Some(binding) = effect.binding_name() {
                    failed_bindings.insert(binding);
                }
            } else {
                // Delete the old resource
                let identifier = from.identifier.as_deref().unwrap_or("");
                match provider.delete(id, identifier, lifecycle).await {
                    Ok(()) => {
                        let (final_state, rename_failed) = finalize_cbd_rename(
                            provider,
                            id,
                            to,
                            &state,
                            temporary_name,
                            permanent_name_overrides,
                            observer,
                        )
                        .await;

                        applied_states.insert(to.id.clone(), final_state);

                        if rename_failed {
                            observer.on_event(&ExecutionEvent::EffectFailed {
                                effect,
                                error: "rename failed",
                                duration: started.elapsed(),
                            });
                            *failure_count += 1;
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        } else {
                            observer.on_event(&ExecutionEvent::EffectSucceeded {
                                effect,
                                state: None,
                                duration: started.elapsed(),
                            });
                            *success_count += 1;
                        }
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &error_str,
                            duration: started.elapsed(),
                        });
                        *failure_count += 1;
                        queue_state_refresh(pending_refreshes, &to.id, state.identifier.as_deref());
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect,
                error: &error_str,
                duration: started.elapsed(),
            });
            *failure_count += 1;
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
        }
    }
}

/// Handle CBD rename after delete succeeds.
async fn finalize_cbd_rename(
    provider: &dyn Provider,
    id: &ResourceId,
    to: &Resource,
    state: &State,
    temporary_name: Option<&crate::effect::TemporaryName>,
    permanent_name_overrides: &mut HashMap<ResourceId, HashMap<String, String>>,
    observer: &mut dyn ExecutionObserver,
) -> (State, bool) {
    if let Some(temp) = temporary_name
        && temp.can_rename
    {
        let new_identifier = state.identifier.as_deref().unwrap_or("");
        let mut rename_to = to.clone();
        rename_to.attributes.insert(
            temp.attribute.clone(),
            Value::String(temp.original_value.clone()),
        );
        match provider.update(id, new_identifier, state, &rename_to).await {
            Ok(renamed_state) => {
                observer.on_event(&ExecutionEvent::RenameSucceeded {
                    id,
                    from: &temp.temporary_value,
                    to: &temp.original_value,
                });
                (renamed_state, false)
            }
            Err(e) => {
                let error_str = e.to_string();
                observer.on_event(&ExecutionEvent::RenameFailed {
                    id,
                    error: &error_str,
                });
                (state.clone(), true)
            }
        }
    } else {
        // Track permanent name override for can_rename=false
        if let Some(temp) = temporary_name
            && !temp.can_rename
        {
            let mut overrides = HashMap::new();
            overrides.insert(temp.attribute.clone(), temp.temporary_value.clone());
            permanent_name_overrides.insert(to.id.clone(), overrides);
        }
        (state.clone(), false)
    }
}

// ---------------------------------------------------------------------------
// DBD Replace (delete-before-destroy) - sequential path
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn execute_dbd_replace(
    provider: &dyn Provider,
    effect: &Effect,
    id: &ResourceId,
    from: &State,
    to: &Resource,
    lifecycle: &crate::resource::LifecycleConfig,
    binding_map: &mut HashMap<String, HashMap<String, Value>>,
    applied_states: &mut HashMap<ResourceId, State>,
    failed_bindings: &mut HashSet<String>,
    pending_refreshes: &mut HashMap<ResourceId, String>,
    success_count: &mut usize,
    failure_count: &mut usize,
    started: Instant,
    observer: &mut dyn ExecutionObserver,
) {
    let identifier = from.identifier.as_deref().unwrap_or("");
    match provider.delete(id, identifier, lifecycle).await {
        Ok(()) => {
            let resolved = resolve_resource(to, binding_map);
            match provider.create(&resolved).await {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect,
                        state: Some(&state),
                        duration: started.elapsed(),
                    });
                    *success_count += 1;
                    applied_states.insert(to.id.clone(), state.clone());
                    update_binding_map(binding_map, &resolved.attributes, &state);
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                    });
                    *failure_count += 1;
                    queue_state_refresh(pending_refreshes, &to.id, Some(identifier));
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect,
                error: &error_str,
                duration: started.elapsed(),
            });
            *failure_count += 1;
            queue_state_refresh(pending_refreshes, id, Some(identifier));
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Effect execution: phased path (interdependent replaces)
// ---------------------------------------------------------------------------

/// Execute effects with dependency-aware ordering for interdependent Replace effects.
///
/// Decomposes Replace effects into phases:
/// 1. Non-Replace effects in original order
/// 2. CBD creates in forward dependency order (parents first)
/// 3. All deletes in reverse dependency order (dependents first)
/// 4. Non-CBD creates in forward dependency order (parents first)
async fn execute_effects_phased(
    provider: &dyn Provider,
    input: &mut ExecutionInput<'_>,
    observer: &mut dyn ExecutionObserver,
) -> ExecutionResult {
    let mut success_count = 0;
    let mut failure_count = 0;
    let skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let mut permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();

    let effects = input.plan.effects();
    let replace_bindings = collect_replace_bindings(effects);
    let sorted_indices = topological_sort_replaces(effects, &replace_bindings);

    // Phase 1: Non-Replace effects in original order
    for effect in effects {
        if matches!(effect, Effect::Replace { .. }) {
            continue;
        }

        if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
            let reason = format!("dependency '{}' failed", failed_dep);
            observer.on_event(&ExecutionEvent::EffectSkipped {
                effect,
                reason: &reason,
            });
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
            continue;
        }

        let started = Instant::now();
        observer.on_event(&ExecutionEvent::EffectStarted { effect });

        match effect {
            Effect::Create(resource) => {
                let resolved = resolve_resource(resource, &input.binding_map);
                match provider.create(&resolved).await {
                    Ok(state) => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                        });
                        success_count += 1;
                        applied_states.insert(resource.id.clone(), state.clone());
                        update_binding_map(&mut input.binding_map, &resolved.attributes, &state);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &error_str,
                            duration: started.elapsed(),
                        });
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Update { id, from, to, .. } => {
                let resolve_source = input.unresolved_resources.get(id).unwrap_or(to);
                let resolved_to =
                    resolve_resource_with_source(to, resolve_source, &input.binding_map);
                let identifier = from.identifier.as_deref().unwrap_or("");
                match provider.update(id, identifier, from, &resolved_to).await {
                    Ok(state) => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                        });
                        success_count += 1;
                        applied_states.insert(id.clone(), state.clone());
                        update_binding_map(&mut input.binding_map, &resolved_to.attributes, &state);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &error_str,
                            duration: started.elapsed(),
                        });
                        failure_count += 1;
                        queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
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
                    });
                    success_count += 1;
                    successfully_deleted.insert(id.clone());
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                    });
                    failure_count += 1;
                    queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                }
            },
            Effect::Read { .. } => {}
            Effect::Replace { .. } => unreachable!(),
        }
    }

    // Phase 2: CBD creates in forward dependency order (parents first)
    let mut cbd_create_states: HashMap<usize, State> = HashMap::new();
    let mut replace_start_times: HashMap<usize, Instant> = HashMap::new();
    for &idx in &sorted_indices {
        let effect = &effects[idx];
        if let Effect::Replace {
            to,
            lifecycle,
            cascading_updates,
            ..
        } = effect
            && lifecycle.create_before_destroy
        {
            if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                let reason = format!("dependency '{}' failed", failed_dep);
                observer.on_event(&ExecutionEvent::EffectSkipped {
                    effect,
                    reason: &reason,
                });
                if let Some(binding) = effect.binding_name() {
                    failed_bindings.insert(binding);
                }
                continue;
            }

            let started = Instant::now();
            replace_start_times.insert(idx, started);
            observer.on_event(&ExecutionEvent::EffectStarted { effect });

            let resolve_source = input.unresolved_resources.get(&to.id).unwrap_or(to);
            let resolved = resolve_resource_with_source(to, resolve_source, &input.binding_map);

            match provider.create(&resolved).await {
                Ok(state) => {
                    update_binding_map(&mut input.binding_map, &resolved.attributes, &state);

                    let mut cascade_failed = false;
                    for cascade in cascading_updates {
                        let resolved_to = resolve_resource(&cascade.to, &input.binding_map);
                        let cascade_identifier = cascade.from.identifier.as_deref().unwrap_or("");
                        match provider
                            .update(&cascade.id, cascade_identifier, &cascade.from, &resolved_to)
                            .await
                        {
                            Ok(cascade_state) => {
                                observer.on_event(&ExecutionEvent::CascadeUpdateSucceeded {
                                    id: &cascade.id,
                                });
                                applied_states.insert(cascade.id.clone(), cascade_state.clone());
                                update_binding_map(
                                    &mut input.binding_map,
                                    &resolved_to.attributes,
                                    &cascade_state,
                                );
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::CascadeUpdateFailed {
                                    id: &cascade.id,
                                    error: &error_str,
                                });
                                queue_state_refresh(
                                    &mut pending_refreshes,
                                    &cascade.id,
                                    Some(cascade_identifier),
                                );
                                cascade_failed = true;
                                failure_count += 1;
                                break;
                            }
                        }
                    }

                    if cascade_failed {
                        queue_state_refresh(
                            &mut pending_refreshes,
                            &to.id,
                            state.identifier.as_deref(),
                        );
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    } else {
                        cbd_create_states.insert(idx, state);
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                    });
                    failure_count += 1;
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                }
            }
        }
    }

    // Phase 3: All deletes in reverse dependency order (dependents first)
    for &idx in sorted_indices.iter().rev() {
        let effect = &effects[idx];
        if let Effect::Replace {
            id,
            from,
            lifecycle,
            ..
        } = effect
        {
            if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                let reason = format!("dependency '{}' failed", failed_dep);
                observer.on_event(&ExecutionEvent::EffectSkipped {
                    effect,
                    reason: &reason,
                });
                if let Some(binding) = effect.binding_name() {
                    failed_bindings.insert(binding);
                }
                continue;
            }

            // For CBD effects, skip delete if the create phase failed
            if lifecycle.create_before_destroy && !cbd_create_states.contains_key(&idx) {
                continue;
            }

            // For non-CBD replaces, this is where the effect starts
            if !lifecycle.create_before_destroy {
                let started = Instant::now();
                replace_start_times.insert(idx, started);
                observer.on_event(&ExecutionEvent::EffectStarted { effect });
            }

            let identifier = from.identifier.as_deref().unwrap_or("");
            match provider.delete(id, identifier, lifecycle).await {
                Ok(()) => {
                    // Delete succeeded
                }
                Err(e) => {
                    let started = replace_start_times
                        .get(&idx)
                        .copied()
                        .unwrap_or_else(Instant::now);
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect,
                        error: &error_str,
                        duration: started.elapsed(),
                    });
                    failure_count += 1;
                    queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                    // For CBD, save the already-created resource state even though delete failed
                    if lifecycle.create_before_destroy
                        && let Some(state) = cbd_create_states.remove(&idx)
                    {
                        let to = match effect {
                            Effect::Replace { to, .. } => to,
                            _ => unreachable!(),
                        };
                        queue_state_refresh(
                            &mut pending_refreshes,
                            &to.id,
                            state.identifier.as_deref(),
                        );
                    }
                }
            }
        }
    }

    // Phase 4: Non-CBD creates and CBD finalization in forward dependency order
    for &idx in &sorted_indices {
        let effect = &effects[idx];
        if let Effect::Replace {
            id,
            to,
            lifecycle,
            temporary_name,
            ..
        } = effect
        {
            let started = replace_start_times
                .get(&idx)
                .copied()
                .unwrap_or_else(Instant::now);

            if lifecycle.create_before_destroy {
                // CBD: already created in phase 2, handle rename and finalize
                if let Some(state) = cbd_create_states.remove(&idx) {
                    let (final_state, rename_failed) = finalize_cbd_rename(
                        provider,
                        id,
                        to,
                        &state,
                        temporary_name.as_ref(),
                        &mut permanent_name_overrides,
                        observer,
                    )
                    .await;

                    applied_states.insert(to.id.clone(), final_state);

                    if rename_failed {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: "rename failed",
                            duration: started.elapsed(),
                        });
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    } else {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: None,
                            duration: started.elapsed(),
                        });
                        success_count += 1;
                    }
                }
            } else {
                // Non-CBD: create the new resource now (after delete in phase 3)
                if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                    let reason = format!("dependency '{}' failed", failed_dep);
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect,
                        reason: &reason,
                    });
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                    continue;
                }

                // Check if this effect's own delete failed
                if let Some(binding) = effect.binding_name()
                    && failed_bindings.contains(&binding)
                {
                    continue;
                }

                let resolve_source = input.unresolved_resources.get(&to.id).unwrap_or(to);
                let resolved = resolve_resource_with_source(to, resolve_source, &input.binding_map);

                match provider.create(&resolved).await {
                    Ok(state) => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: Some(&state),
                            duration: started.elapsed(),
                        });
                        success_count += 1;
                        applied_states.insert(to.id.clone(), state.clone());
                        update_binding_map(&mut input.binding_map, &resolved.attributes, &state);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: &error_str,
                            duration: started.elapsed(),
                        });
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
        }
    }

    let failed_refreshes = refresh_pending_states(
        provider,
        &mut input.current_states,
        &pending_refreshes,
        observer,
    )
    .await;

    ExecutionResult {
        success_count,
        failure_count,
        skip_count,
        applied_states,
        successfully_deleted,
        permanent_name_overrides,
        current_states: input.current_states.clone(),
        failed_refreshes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{BoxFuture, ProviderError, ProviderResult};
    use crate::resource::LifecycleConfig;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // Mock Provider
    // -----------------------------------------------------------------------

    struct MockProvider {
        create_results: Mutex<Vec<ProviderResult<State>>>,
        delete_results: Mutex<Vec<ProviderResult<()>>>,
        update_results: Mutex<Vec<ProviderResult<State>>>,
        read_results: Mutex<Vec<ProviderResult<State>>>,
        /// Records calls in order: ("create"|"delete"|"update"|"read", resource_id_string)
        call_log: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                create_results: Mutex::new(Vec::new()),
                delete_results: Mutex::new(Vec::new()),
                update_results: Mutex::new(Vec::new()),
                read_results: Mutex::new(Vec::new()),
                call_log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn push_create(&self, result: ProviderResult<State>) {
            self.create_results.lock().unwrap().push(result);
        }

        fn push_delete(&self, result: ProviderResult<()>) {
            self.delete_results.lock().unwrap().push(result);
        }

        #[allow(dead_code)]
        fn push_update(&self, result: ProviderResult<State>) {
            self.update_results.lock().unwrap().push(result);
        }

        #[allow(dead_code)]
        fn push_read(&self, result: ProviderResult<State>) {
            self.read_results.lock().unwrap().push(result);
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.call_log.lock().unwrap().clone()
        }
    }

    impl Provider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id_str = id.to_string();
            self.call_log
                .lock()
                .unwrap()
                .push(("read".to_string(), id_str));
            let result = self.read_results.lock().unwrap().remove(0);
            Box::pin(async move { result })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let id_str = resource.id.to_string();
            self.call_log
                .lock()
                .unwrap()
                .push(("create".to_string(), id_str));
            let result = self.create_results.lock().unwrap().remove(0);
            Box::pin(async move { result })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _from: &State,
            _to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id_str = id.to_string();
            self.call_log
                .lock()
                .unwrap()
                .push(("update".to_string(), id_str));
            let result = self.update_results.lock().unwrap().remove(0);
            Box::pin(async move { result })
        }

        fn delete(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            let id_str = id.to_string();
            self.call_log
                .lock()
                .unwrap()
                .push(("delete".to_string(), id_str));
            let result = self.delete_results.lock().unwrap().remove(0);
            Box::pin(async move { result })
        }
    }

    // -----------------------------------------------------------------------
    // Mock Observer
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct MockObserver {
        events: Vec<String>,
    }

    impl ExecutionObserver for MockObserver {
        fn on_event(&mut self, event: &ExecutionEvent) {
            let msg = match event {
                ExecutionEvent::EffectStarted { effect } => {
                    format!("started:{}", effect.resource_id())
                }
                ExecutionEvent::EffectSucceeded { effect, .. } => {
                    format!("succeeded:{}", effect.resource_id())
                }
                ExecutionEvent::EffectFailed { effect, error, .. } => {
                    format!("failed:{}:{}", effect.resource_id(), error)
                }
                ExecutionEvent::EffectSkipped { effect, reason } => {
                    format!("skipped:{}:{}", effect.resource_id(), reason)
                }
                ExecutionEvent::CascadeUpdateSucceeded { id } => {
                    format!("cascade_ok:{}", id)
                }
                ExecutionEvent::CascadeUpdateFailed { id, error } => {
                    format!("cascade_fail:{}:{}", id, error)
                }
                ExecutionEvent::RenameSucceeded { id, from, to } => {
                    format!("rename_ok:{}:{}:{}", id, from, to)
                }
                ExecutionEvent::RenameFailed { id, error } => {
                    format!("rename_fail:{}:{}", id, error)
                }
                ExecutionEvent::RefreshStarted => "refresh_started".to_string(),
                ExecutionEvent::RefreshSucceeded { id } => {
                    format!("refresh_ok:{}", id)
                }
                ExecutionEvent::RefreshFailed { id, error } => {
                    format!("refresh_fail:{}:{}", id, error)
                }
            };
            self.events.push(msg);
        }
    }

    // -----------------------------------------------------------------------
    // Helper functions
    // -----------------------------------------------------------------------

    fn make_resource(binding: &str, deps: &[&str]) -> Resource {
        let mut r = Resource::new("test", binding);
        r.attributes
            .insert("_binding".to_string(), Value::String(binding.to_string()));
        for dep in deps {
            r.attributes.insert(
                format!("ref_{}", dep),
                Value::ResourceRef {
                    binding_name: dep.to_string(),
                    attribute_name: "id".to_string(),
                },
            );
        }
        // Save dependency bindings as metadata (normally done by resolver)
        if !deps.is_empty() {
            let dep_list: Vec<Value> = deps.iter().map(|d| Value::String(d.to_string())).collect();
            r.attributes
                .insert("_dependency_bindings".to_string(), Value::List(dep_list));
        }
        r
    }

    fn ok_state(id: &ResourceId) -> State {
        State::existing(id.clone(), HashMap::new()).with_identifier("id-123")
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_simple_create() {
        let provider = MockProvider::new();
        let resource = make_resource("a", &[]);
        let rid = resource.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(resource));

        provider.push_create(Ok(ok_state(&rid)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 1);
        assert_eq!(result.failure_count, 0);
        assert!(observer.events.iter().any(|e| e.starts_with("succeeded:")));
    }

    #[tokio::test]
    async fn test_simple_delete() {
        let provider = MockProvider::new();
        let rid = ResourceId::new("test", "a");

        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: rid.clone(),
            identifier: "id-123".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: None,
            dependencies: HashSet::new(),
        });

        provider.push_delete(Ok(()));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 1);
        assert!(result.successfully_deleted.contains(&rid));
    }

    #[tokio::test]
    async fn test_failed_effect_propagates_to_dependent() {
        let provider = MockProvider::new();
        let ra = make_resource("a", &[]);
        let rb = make_resource("b", &["a"]);
        let _rid_a = ra.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(ra));
        plan.add(Effect::Create(rb));

        // First create fails
        provider.push_create(Err(ProviderError::new("create failed")));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.failure_count, 1);
        assert_eq!(result.skip_count, 1);
        assert!(observer.events.iter().any(|e| e.contains("failed:")));
        assert!(
            observer
                .events
                .iter()
                .any(|e| e.contains("skipped:") && e.contains("dependency 'a' failed"))
        );
    }

    #[tokio::test]
    async fn test_cbd_creates_before_deletes() {
        // CBD Replace: create should happen before delete
        let provider = MockProvider::new();
        let rid = ResourceId::new("test", "a");
        let from = State::existing(rid.clone(), HashMap::new()).with_identifier("old-id");
        let to = Resource::new("test", "a");

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: rid.clone(),
            from: Box::new(from),
            to,
            lifecycle: LifecycleConfig {
                create_before_destroy: true,
                ..Default::default()
            },
            changed_create_only: vec!["attr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        provider.push_create(Ok(ok_state(&rid)));
        provider.push_delete(Ok(()));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 1);
        assert_eq!(result.failure_count, 0);

        let calls = provider.calls();
        assert_eq!(calls[0].0, "create");
        assert_eq!(calls[1].0, "delete");
    }

    #[tokio::test]
    async fn test_dbd_deletes_before_creates() {
        // Non-CBD Replace: delete should happen before create
        let provider = MockProvider::new();
        let rid = ResourceId::new("test", "a");
        let from = State::existing(rid.clone(), HashMap::new()).with_identifier("old-id");
        let to = Resource::new("test", "a");

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: rid.clone(),
            from: Box::new(from),
            to,
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["attr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        provider.push_delete(Ok(()));
        provider.push_create(Ok(ok_state(&rid)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 1);
        assert_eq!(result.failure_count, 0);

        let calls = provider.calls();
        assert_eq!(calls[0].0, "delete");
        assert_eq!(calls[1].0, "create");
    }

    #[tokio::test]
    async fn test_phased_cbd_creates_in_forward_order_deletes_in_reverse() {
        // Two interdependent replaces: vpc (parent) and subnet (depends on vpc)
        // Both are CBD. Expected order:
        //   Phase 2: create vpc, create subnet (forward)
        //   Phase 3: delete subnet, delete vpc (reverse)
        //   Phase 4: finalize (success events)
        let provider = MockProvider::new();
        let vpc_id = ResourceId::new("test", "vpc");
        let subnet_id = ResourceId::new("test", "subnet");

        let vpc_from = State::existing(vpc_id.clone(), HashMap::new()).with_identifier("vpc-old");
        let mut vpc_to = Resource::new("test", "vpc");
        vpc_to
            .attributes
            .insert("_binding".to_string(), Value::String("vpc".to_string()));

        let subnet_from =
            State::existing(subnet_id.clone(), HashMap::new()).with_identifier("subnet-old");
        let mut subnet_to = Resource::new("test", "subnet");
        subnet_to
            .attributes
            .insert("_binding".to_string(), Value::String("subnet".to_string()));
        subnet_to.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![Value::String("vpc".to_string())]),
        );

        let cbd_lifecycle = LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        };

        let mut plan = Plan::new();
        // Order in plan: vpc first, subnet second
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(vpc_from),
            to: vpc_to,
            lifecycle: cbd_lifecycle.clone(),
            changed_create_only: vec!["attr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });
        plan.add(Effect::Replace {
            id: subnet_id.clone(),
            from: Box::new(subnet_from),
            to: subnet_to,
            lifecycle: cbd_lifecycle,
            changed_create_only: vec!["attr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        // Phase 2: create vpc, create subnet
        provider.push_create(Ok(ok_state(&vpc_id)));
        provider.push_create(Ok(ok_state(&subnet_id)));
        // Phase 3: delete subnet (reverse), delete vpc (reverse)
        provider.push_delete(Ok(()));
        provider.push_delete(Ok(()));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 2);
        assert_eq!(result.failure_count, 0);

        let calls = provider.calls();
        // Phase 2: creates in forward order (vpc before subnet)
        assert_eq!(calls[0], ("create".to_string(), vpc_id.to_string()));
        assert_eq!(calls[1], ("create".to_string(), subnet_id.to_string()));
        // Phase 3: deletes in reverse order (subnet before vpc)
        assert_eq!(calls[2], ("delete".to_string(), subnet_id.to_string()));
        assert_eq!(calls[3], ("delete".to_string(), vpc_id.to_string()));
    }

    #[tokio::test]
    async fn test_phased_noncbd_creates_after_deletes() {
        // Two interdependent non-CBD replaces: vpc (parent) and subnet (depends on vpc)
        // Expected order:
        //   Phase 3: delete subnet, delete vpc (reverse dependency)
        //   Phase 4: create vpc, create subnet (forward dependency)
        let provider = MockProvider::new();
        let vpc_id = ResourceId::new("test", "vpc");
        let subnet_id = ResourceId::new("test", "subnet");

        let vpc_from = State::existing(vpc_id.clone(), HashMap::new()).with_identifier("vpc-old");
        let mut vpc_to = Resource::new("test", "vpc");
        vpc_to
            .attributes
            .insert("_binding".to_string(), Value::String("vpc".to_string()));

        let subnet_from =
            State::existing(subnet_id.clone(), HashMap::new()).with_identifier("subnet-old");
        let mut subnet_to = Resource::new("test", "subnet");
        subnet_to
            .attributes
            .insert("_binding".to_string(), Value::String("subnet".to_string()));
        subnet_to.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![Value::String("vpc".to_string())]),
        );

        let dbd_lifecycle = LifecycleConfig::default();

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: vpc_id.clone(),
            from: Box::new(vpc_from),
            to: vpc_to,
            lifecycle: dbd_lifecycle.clone(),
            changed_create_only: vec!["attr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });
        plan.add(Effect::Replace {
            id: subnet_id.clone(),
            from: Box::new(subnet_from),
            to: subnet_to,
            lifecycle: dbd_lifecycle,
            changed_create_only: vec!["attr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        // Phase 3: delete subnet, delete vpc
        provider.push_delete(Ok(()));
        provider.push_delete(Ok(()));
        // Phase 4: create vpc, create subnet
        provider.push_create(Ok(ok_state(&vpc_id)));
        provider.push_create(Ok(ok_state(&subnet_id)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 2);
        assert_eq!(result.failure_count, 0);

        let calls = provider.calls();
        // Phase 3: deletes in reverse dependency order
        assert_eq!(calls[0], ("delete".to_string(), subnet_id.to_string()));
        assert_eq!(calls[1], ("delete".to_string(), vpc_id.to_string()));
        // Phase 4: creates in forward dependency order
        assert_eq!(calls[2], ("create".to_string(), vpc_id.to_string()));
        assert_eq!(calls[3], ("create".to_string(), subnet_id.to_string()));
    }

    #[tokio::test]
    async fn test_observer_events_emitted_correctly() {
        let provider = MockProvider::new();
        let resource = make_resource("a", &[]);
        let rid = resource.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(resource));

        provider.push_create(Ok(ok_state(&rid)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        execute_plan(&provider, input, &mut observer).await;

        assert_eq!(observer.events.len(), 2);
        assert!(observer.events[0].starts_with("started:"));
        assert!(observer.events[1].starts_with("succeeded:"));
    }

    #[tokio::test]
    async fn test_read_effect_is_no_op() {
        let provider = MockProvider::new();
        let resource = Resource::new("test", "data").with_read_only(true);

        let mut plan = Plan::new();
        plan.add(Effect::Read { resource });

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let mut observer = MockObserver::default();
        let result = execute_plan(&provider, input, &mut observer).await;

        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 0);
        assert!(provider.calls().is_empty());
    }
}
