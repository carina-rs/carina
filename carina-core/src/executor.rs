//! Plan executor - Executes a Plan by dispatching Effects to a Provider.
//!
//! This module contains the core execution logic extracted from the CLI apply command.
//! It uses an `ExecutionObserver` trait for UI separation, allowing the CLI to provide
//! colored progress output while keeping the execution logic testable.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};

use crate::deps::{find_failed_dependency, get_resource_dependencies};
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

/// Progress information for effect execution.
#[derive(Debug, Clone, Copy)]
pub struct ProgressInfo {
    /// Number of effects completed so far (including this one).
    pub completed: usize,
    /// Total number of actionable effects (excluding Read).
    pub total: usize,
}

/// Events emitted during plan execution.
pub enum ExecutionEvent<'a> {
    /// An effect is waiting for dependencies to complete before it can start.
    Waiting {
        effect: &'a Effect,
        /// Binding names of the dependencies that have not yet completed.
        pending_dependencies: Vec<String>,
    },
    EffectStarted {
        effect: &'a Effect,
    },
    EffectSucceeded {
        effect: &'a Effect,
        state: Option<&'a State>,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectFailed {
        effect: &'a Effect,
        error: &'a str,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectSkipped {
        effect: &'a Effect,
        reason: &'a str,
        progress: ProgressInfo,
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
///
/// Implementations must handle concurrent calls from parallel effect execution.
/// Use interior mutability (e.g., `Mutex`) if mutable state is needed.
pub trait ExecutionObserver: Send + Sync {
    fn on_event(&self, event: &ExecutionEvent);
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
    observer: &dyn ExecutionObserver,
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
    observer: &dyn ExecutionObserver,
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

/// Count the number of actionable effects (excluding Read).
fn count_actionable_effects(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| !matches!(e, Effect::Read { .. }))
        .count()
}

/// Build dependency levels from effects.
///
/// Groups effects into levels where all effects in a level have their
/// dependencies satisfied by effects in earlier levels. Effects within
/// the same level can be executed concurrently.
#[cfg(test)]
fn build_dependency_levels(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> Vec<Vec<usize>> {
    // Build binding -> effect index mapping
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        if let Some(binding) = effect.binding_name() {
            binding_to_idx.insert(binding, idx);
        }
    }

    // For each effect, compute which other effect indices it depends on.
    // Check both the effect's resource and the unresolved resource (which may
    // still have ResourceRef values before they were resolved to plain strings).
    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        let mut dep_indices = HashSet::new();
        if let Some(resource) = effect.resource() {
            let dep_bindings = get_resource_dependencies(resource);
            for dep_binding in &dep_bindings {
                if let Some(&dep_idx) = binding_to_idx.get(dep_binding) {
                    dep_indices.insert(dep_idx);
                }
            }
            // Also check unresolved source for dependencies (ResourceRef values
            // may have been resolved to plain strings in the effect's resource)
            if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                let unresolved_deps = get_resource_dependencies(unresolved);
                for dep_binding in &unresolved_deps {
                    if let Some(&dep_idx) = binding_to_idx.get(dep_binding) {
                        dep_indices.insert(dep_idx);
                    }
                }
            }
        }
        deps_of.insert(idx, dep_indices);
    }

    // Assign levels: each effect's level is max(deps' levels) + 1, or 0 if no deps
    let mut levels: HashMap<usize, usize> = HashMap::new();
    let mut assigned = HashSet::new();

    // Iteratively assign levels (handle forward references)
    loop {
        let mut progress = false;
        for idx in 0..effects.len() {
            if assigned.contains(&idx) {
                continue;
            }
            let deps = &deps_of[&idx];
            if deps.iter().all(|d| assigned.contains(d)) {
                let level = deps.iter().map(|d| levels[d] + 1).max().unwrap_or(0);
                levels.insert(idx, level);
                assigned.insert(idx);
                progress = true;
            }
        }
        if !progress {
            // Remaining effects (cycles or Read) get assigned to level 0
            for idx in 0..effects.len() {
                if !assigned.contains(&idx) {
                    levels.insert(idx, 0);
                    assigned.insert(idx);
                }
            }
            break;
        }
        if assigned.len() == effects.len() {
            break;
        }
    }

    // Group by level
    let max_level = levels.values().copied().max().unwrap_or(0);
    let mut result: Vec<Vec<usize>> = vec![Vec::new(); max_level + 1];
    for (idx, &level) in &levels {
        result[level].push(*idx);
    }

    // Sort indices within each level for deterministic ordering
    for group in &mut result {
        group.sort();
    }

    result
}

/// Result of executing a single effect.
enum SingleEffectResult {
    Success {
        state: Option<State>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
    },
    Failure {
        binding: Option<String>,
        refresh: Option<(ResourceId, String)>,
    },
    Deleted {
        resource_id: ResourceId,
    },
    Replace {
        success: bool,
        state: Option<State>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
        binding: Option<String>,
        refreshes: Vec<(ResourceId, String)>,
        permanent_overrides: Option<(ResourceId, HashMap<String, String>)>,
    },
    ReadNoOp,
}

/// Build a dependency map: for each effect index, which other effect indices it depends on.
fn build_dependency_map(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> HashMap<usize, HashSet<usize>> {
    // Build binding -> effect index mapping
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        if let Some(binding) = effect.binding_name() {
            binding_to_idx.insert(binding, idx);
        }
    }

    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        let mut dep_indices = HashSet::new();
        if let Some(resource) = effect.resource() {
            let dep_bindings = get_resource_dependencies(resource);
            for dep_binding in &dep_bindings {
                if let Some(&dep_idx) = binding_to_idx.get(dep_binding) {
                    dep_indices.insert(dep_idx);
                }
            }
            if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                let unresolved_deps = get_resource_dependencies(unresolved);
                for dep_binding in &unresolved_deps {
                    if let Some(&dep_idx) = binding_to_idx.get(dep_binding) {
                        dep_indices.insert(dep_idx);
                    }
                }
            }
        }
        deps_of.insert(idx, dep_indices);
    }
    deps_of
}

/// Execute effects with fine-grained scheduling.
///
/// Instead of grouping effects into dependency levels and waiting for all
/// effects in a level to complete, this spawns each effect as soon as all
/// its dependencies have completed. This allows dependent effects to start
/// immediately when their specific dependencies finish, even if other
/// independent effects in the same "level" are still running.
async fn execute_effects_sequential(
    provider: &dyn Provider,
    input: &mut ExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
) -> ExecutionResult {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let mut permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();

    let effects = input.plan.effects();
    let total = count_actionable_effects(effects);
    let completed = AtomicUsize::new(0);

    let deps_of = build_dependency_map(effects, input.unresolved_resources);

    // Build effect index -> binding name mapping for resolving dependency names
    let idx_to_binding: HashMap<usize, String> = effects
        .iter()
        .enumerate()
        .filter_map(|(idx, effect)| effect.binding_name().map(|b| (idx, b)))
        .collect();

    // Track which effect indices have completed (successfully or not)
    let mut completed_indices: HashSet<usize> = HashSet::new();
    // Track which effect indices have been dispatched (spawned or skipped)
    let mut dispatched: HashSet<usize> = HashSet::new();
    // All actionable effect indices (excluding Read)
    let actionable_indices: Vec<usize> = (0..effects.len())
        .filter(|&idx| !matches!(&effects[idx], Effect::Read { .. }))
        .collect();

    // Mark Read effects as completed (they are no-ops but may be dependencies)
    for (idx, effect) in effects.iter().enumerate() {
        if matches!(effect, Effect::Read { .. }) {
            completed_indices.insert(idx);
            dispatched.insert(idx);
        }
    }

    let mut in_flight = FuturesUnordered::new();

    loop {
        // Find newly ready effects: all deps completed and not yet dispatched
        let mut newly_ready: Vec<usize> = Vec::new();
        for &idx in &actionable_indices {
            if dispatched.contains(&idx) {
                continue;
            }
            let deps = &deps_of[&idx];
            if deps.iter().all(|d| completed_indices.contains(d)) {
                newly_ready.push(idx);
            }
        }
        // Sort for deterministic ordering
        newly_ready.sort();

        // Emit Waiting events for effects that have unmet dependencies
        for &idx in &actionable_indices {
            if dispatched.contains(&idx) || newly_ready.contains(&idx) {
                continue;
            }
            let deps = &deps_of[&idx];
            let pending: Vec<String> = deps
                .iter()
                .filter(|d| !completed_indices.contains(d))
                .filter_map(|d| idx_to_binding.get(d).cloned())
                .collect();
            if !pending.is_empty() {
                // Emit on every iteration to update the pending dependency list
                observer.on_event(&ExecutionEvent::Waiting {
                    effect: &effects[idx],
                    pending_dependencies: pending,
                });
            }
        }

        // Process newly ready effects: skip those with failed deps, spawn the rest
        for idx in newly_ready {
            dispatched.insert(idx);
            let effect = &effects[idx];

            if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
                let reason = format!("dependency '{}' failed", failed_dep);
                observer.on_event(&ExecutionEvent::EffectSkipped {
                    effect,
                    reason: &reason,
                    progress: ProgressInfo {
                        completed: c,
                        total,
                    },
                });
                skip_count += 1;
                if let Some(binding) = effect.binding_name() {
                    failed_bindings.insert(binding);
                }
                completed_indices.insert(idx);
                continue;
            }

            // Snapshot binding_map for this effect's resolution
            let binding_snapshot = input.binding_map.clone();
            let unresolved = &input.unresolved_resources;
            let completed_ref = &completed;

            in_flight.push(async move {
                let result = match effect {
                    Effect::Create(resource) => {
                        let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                        let started = Instant::now();
                        observer.on_event(&ExecutionEvent::EffectStarted { effect });
                        let resolved = resolve_resource(resource, &binding_snapshot);
                        match provider.create(&resolved).await {
                            Ok(state) => {
                                observer.on_event(&ExecutionEvent::EffectSucceeded {
                                    effect,
                                    state: Some(&state),
                                    duration: started.elapsed(),
                                    progress: ProgressInfo {
                                        completed: c,
                                        total,
                                    },
                                });
                                SingleEffectResult::Success {
                                    state: Some(state),
                                    resource_id: resource.id.clone(),
                                    resolved_attrs: Some(resolved.attributes),
                                }
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::EffectFailed {
                                    effect,
                                    error: &error_str,
                                    duration: started.elapsed(),
                                    progress: ProgressInfo {
                                        completed: c,
                                        total,
                                    },
                                });
                                SingleEffectResult::Failure {
                                    binding: effect.binding_name(),
                                    refresh: None,
                                }
                            }
                        }
                    }
                    Effect::Update { id, from, to, .. } => {
                        let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                        let started = Instant::now();
                        observer.on_event(&ExecutionEvent::EffectStarted { effect });
                        let resolve_source = unresolved.get(id).unwrap_or(to);
                        let resolved_to =
                            resolve_resource_with_source(to, resolve_source, &binding_snapshot);
                        let identifier = from.identifier.as_deref().unwrap_or("");
                        match provider.update(id, identifier, from, &resolved_to).await {
                            Ok(state) => {
                                observer.on_event(&ExecutionEvent::EffectSucceeded {
                                    effect,
                                    state: Some(&state),
                                    duration: started.elapsed(),
                                    progress: ProgressInfo {
                                        completed: c,
                                        total,
                                    },
                                });
                                SingleEffectResult::Success {
                                    state: Some(state),
                                    resource_id: id.clone(),
                                    resolved_attrs: Some(resolved_to.attributes),
                                }
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::EffectFailed {
                                    effect,
                                    error: &error_str,
                                    duration: started.elapsed(),
                                    progress: ProgressInfo {
                                        completed: c,
                                        total,
                                    },
                                });
                                SingleEffectResult::Failure {
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
                    } => {
                        let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                        let started = Instant::now();
                        let progress = ProgressInfo {
                            completed: c,
                            total,
                        };
                        observer.on_event(&ExecutionEvent::EffectStarted { effect });
                        match provider.delete(id, identifier, lifecycle).await {
                            Ok(()) => {
                                observer.on_event(&ExecutionEvent::EffectSucceeded {
                                    effect,
                                    state: None,
                                    duration: started.elapsed(),
                                    progress,
                                });
                                SingleEffectResult::Deleted {
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
                                SingleEffectResult::Failure {
                                    binding: None,
                                    refresh: Some((id.clone(), identifier.clone())),
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
                        let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                        let started = Instant::now();
                        let progress = ProgressInfo {
                            completed: c,
                            total,
                        };
                        observer.on_event(&ExecutionEvent::EffectStarted { effect });

                        execute_replace_parallel(
                            provider,
                            effect,
                            id,
                            from,
                            to,
                            lifecycle,
                            cascading_updates,
                            temporary_name.as_ref(),
                            &binding_snapshot,
                            unresolved,
                            started,
                            progress,
                            observer,
                        )
                        .await
                    }
                    Effect::Read { .. } => SingleEffectResult::ReadNoOp,
                };
                (idx, result)
            });
        }

        // If nothing is in flight, we're done (or stuck in a cycle)
        if in_flight.is_empty() {
            // Check for undispatched effects (would indicate a dependency cycle)
            let remaining = actionable_indices
                .iter()
                .filter(|idx| !dispatched.contains(idx))
                .count();
            if remaining > 0 {
                // Cycle detected: skip remaining effects as failures
                for &idx in &actionable_indices {
                    if !dispatched.contains(&idx) {
                        dispatched.insert(idx);
                        completed_indices.insert(idx);
                        failure_count += 1;
                    }
                }
            }
            break;
        }

        // Wait for the next effect to complete
        let (finished_idx, result) = in_flight.next().await.unwrap();
        completed_indices.insert(finished_idx);

        // Process the result and update shared state immediately
        match result {
            SingleEffectResult::Success {
                state,
                resource_id,
                resolved_attrs,
                ..
            } => {
                success_count += 1;
                if let Some(state) = &state {
                    applied_states.insert(resource_id, state.clone());
                    if let Some(attrs) = &resolved_attrs {
                        update_binding_map(&mut input.binding_map, attrs, state);
                    }
                }
            }
            SingleEffectResult::Failure {
                binding, refresh, ..
            } => {
                failure_count += 1;
                if let Some(binding) = binding {
                    failed_bindings.insert(binding);
                }
                if let Some((id, identifier)) = refresh
                    && !identifier.is_empty()
                {
                    pending_refreshes.insert(id, identifier);
                }
            }
            SingleEffectResult::Deleted { resource_id, .. } => {
                success_count += 1;
                successfully_deleted.insert(resource_id);
            }
            SingleEffectResult::Replace {
                success,
                state,
                resource_id,
                resolved_attrs,
                binding,
                refreshes,
                permanent_overrides,
            } => {
                if let Some(state) = &state {
                    applied_states.insert(resource_id, state.clone());
                    if let Some(attrs) = &resolved_attrs {
                        update_binding_map(&mut input.binding_map, attrs, state);
                    }
                }
                if success {
                    success_count += 1;
                    if let Some((id, overrides)) = permanent_overrides {
                        permanent_name_overrides.insert(id, overrides);
                    }
                } else {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                }
                for (id, identifier) in refreshes {
                    if !identifier.is_empty() {
                        pending_refreshes.insert(id, identifier);
                    }
                }
            }
            SingleEffectResult::ReadNoOp => {}
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
// Replace execution for parallel path
// ---------------------------------------------------------------------------

/// Execute a Replace effect, returning a `SingleEffectResult`.
///
/// This handles both CBD and DBD replace within the parallel execution path.
/// It does not mutate shared state directly; instead returns all data needed
/// for the caller to update shared state after the level completes.
#[allow(clippy::too_many_arguments)]
async fn execute_replace_parallel(
    provider: &dyn Provider,
    effect: &Effect,
    id: &ResourceId,
    from: &State,
    to: &Resource,
    lifecycle: &crate::resource::LifecycleConfig,
    cascading_updates: &[crate::effect::CascadingUpdate],
    temporary_name: Option<&crate::effect::TemporaryName>,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    unresolved: &HashMap<ResourceId, Resource>,
    started: Instant,
    progress: ProgressInfo,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    if lifecycle.create_before_destroy {
        execute_cbd_replace_parallel(
            provider,
            effect,
            id,
            from,
            to,
            lifecycle,
            cascading_updates,
            temporary_name,
            binding_map,
            unresolved,
            started,
            progress,
            observer,
        )
        .await
    } else {
        execute_dbd_replace_parallel(
            provider,
            effect,
            id,
            from,
            to,
            lifecycle,
            binding_map,
            unresolved,
            started,
            progress,
            observer,
        )
        .await
    }
}

/// CBD Replace for the parallel execution path.
#[allow(clippy::too_many_arguments)]
async fn execute_cbd_replace_parallel(
    provider: &dyn Provider,
    effect: &Effect,
    id: &ResourceId,
    from: &State,
    to: &Resource,
    lifecycle: &crate::resource::LifecycleConfig,
    cascading_updates: &[crate::effect::CascadingUpdate],
    temporary_name: Option<&crate::effect::TemporaryName>,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    _unresolved: &HashMap<ResourceId, Resource>,
    started: Instant,
    progress: ProgressInfo,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    let resolved = resolve_resource(to, binding_map);
    let mut refreshes = Vec::new();

    match provider.create(&resolved).await {
        Ok(state) => {
            // Build a local binding map update for cascade resolution
            let mut local_binding_map = binding_map.clone();
            update_binding_map(&mut local_binding_map, &resolved.attributes, &state);

            // Execute cascading updates
            let mut cascade_failed = false;
            for cascade in cascading_updates {
                let resolved_to = resolve_resource(&cascade.to, &local_binding_map);
                let cascade_identifier = cascade.from.identifier.as_deref().unwrap_or("");
                match provider
                    .update(&cascade.id, cascade_identifier, &cascade.from, &resolved_to)
                    .await
                {
                    Ok(cascade_state) => {
                        observer
                            .on_event(&ExecutionEvent::CascadeUpdateSucceeded { id: &cascade.id });
                        update_binding_map(
                            &mut local_binding_map,
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
                        refreshes.push((cascade.id.clone(), cascade_identifier.to_string()));
                        cascade_failed = true;
                        break;
                    }
                }
            }

            if cascade_failed {
                refreshes.push((to.id.clone(), state.identifier.clone().unwrap_or_default()));
                return SingleEffectResult::Replace {
                    success: false,
                    state: None,
                    resource_id: to.id.clone(),
                    resolved_attrs: None,
                    binding: effect.binding_name(),
                    refreshes,
                    permanent_overrides: None,
                };
            }

            // Delete the old resource
            let identifier = from.identifier.as_deref().unwrap_or("");
            match provider.delete(id, identifier, lifecycle).await {
                Ok(()) => {
                    // Handle rename
                    let mut permanent_overrides = None;
                    let mut final_state = state.clone();
                    let mut rename_failed = false;

                    if let Some(temp) = temporary_name
                        && temp.can_rename
                    {
                        let new_identifier = state.identifier.as_deref().unwrap_or("");
                        let mut rename_to = to.clone();
                        rename_to.attributes.insert(
                            temp.attribute.clone(),
                            Value::String(temp.original_value.clone()),
                        );
                        match provider
                            .update(id, new_identifier, &state, &rename_to)
                            .await
                        {
                            Ok(renamed_state) => {
                                observer.on_event(&ExecutionEvent::RenameSucceeded {
                                    id,
                                    from: &temp.temporary_value,
                                    to: &temp.original_value,
                                });
                                final_state = renamed_state;
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::RenameFailed {
                                    id,
                                    error: &error_str,
                                });
                                rename_failed = true;
                            }
                        }
                    } else if let Some(temp) = temporary_name
                        && !temp.can_rename
                    {
                        let mut overrides = HashMap::new();
                        overrides.insert(temp.attribute.clone(), temp.temporary_value.clone());
                        permanent_overrides = Some((to.id.clone(), overrides));
                    }

                    if rename_failed {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect,
                            error: "rename failed",
                            duration: started.elapsed(),
                            progress,
                        });
                        SingleEffectResult::Replace {
                            success: false,
                            state: Some(final_state),
                            resource_id: to.id.clone(),
                            resolved_attrs: Some(resolved.attributes),
                            binding: effect.binding_name(),
                            refreshes,

                            permanent_overrides,
                        }
                    } else {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect,
                            state: None,
                            duration: started.elapsed(),
                            progress,
                        });
                        SingleEffectResult::Replace {
                            success: true,
                            state: Some(final_state),
                            resource_id: to.id.clone(),
                            resolved_attrs: Some(resolved.attributes),
                            binding: None,
                            refreshes,

                            permanent_overrides,
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
                    refreshes.push((to.id.clone(), state.identifier.clone().unwrap_or_default()));
                    SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: to.id.clone(),
                        resolved_attrs: None,
                        binding: effect.binding_name(),
                        refreshes,

                        permanent_overrides: None,
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
                progress,
            });
            SingleEffectResult::Replace {
                success: false,
                state: None,
                resource_id: to.id.clone(),
                resolved_attrs: None,
                binding: effect.binding_name(),
                refreshes,

                permanent_overrides: None,
            }
        }
    }
}

/// DBD Replace for the parallel execution path.
#[allow(clippy::too_many_arguments)]
async fn execute_dbd_replace_parallel(
    provider: &dyn Provider,
    effect: &Effect,
    id: &ResourceId,
    from: &State,
    to: &Resource,
    lifecycle: &crate::resource::LifecycleConfig,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    unresolved: &HashMap<ResourceId, Resource>,
    started: Instant,
    progress: ProgressInfo,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    let identifier = from.identifier.as_deref().unwrap_or("");
    let mut refreshes = Vec::new();

    match provider.delete(id, identifier, lifecycle).await {
        Ok(()) => {
            let resolve_source = unresolved.get(&to.id).unwrap_or(to);
            let resolved = resolve_resource_with_source(to, resolve_source, binding_map);
            match provider.create(&resolved).await {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect,
                        state: Some(&state),
                        duration: started.elapsed(),
                        progress,
                    });
                    SingleEffectResult::Replace {
                        success: true,
                        state: Some(state),
                        resource_id: to.id.clone(),
                        resolved_attrs: Some(resolved.attributes),
                        binding: None,
                        refreshes,

                        permanent_overrides: None,
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
                    refreshes.push((to.id.clone(), identifier.to_string()));
                    SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: to.id.clone(),
                        resolved_attrs: None,
                        binding: effect.binding_name(),
                        refreshes,

                        permanent_overrides: None,
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
                progress,
            });
            refreshes.push((id.clone(), identifier.to_string()));
            SingleEffectResult::Replace {
                success: false,
                state: None,
                resource_id: to.id.clone(),
                resolved_attrs: None,
                binding: effect.binding_name(),
                refreshes,

                permanent_overrides: None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Effect execution: phased path (interdependent replaces)
// ---------------------------------------------------------------------------

/// Build a dependency map for a subset of effects identified by their indices.
///
/// Only considers dependencies between effects in the given subset. Dependencies
/// on effects outside the subset are ignored (assumed already completed).
fn build_phase_dependency_map(
    effects: &[Effect],
    phase_indices: &[usize],
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> HashMap<usize, HashSet<usize>> {
    // Build binding -> effect index mapping for effects in this phase
    let phase_set: HashSet<usize> = phase_indices.iter().copied().collect();
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    for &idx in phase_indices {
        if let Some(binding) = effects[idx].binding_name() {
            binding_to_idx.insert(binding, idx);
        }
    }

    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for &idx in phase_indices {
        let mut dep_indices = HashSet::new();
        let effect = &effects[idx];
        if let Some(resource) = effect.resource() {
            let dep_bindings = get_resource_dependencies(resource);
            for dep_binding in &dep_bindings {
                if let Some(&dep_idx) = binding_to_idx.get(dep_binding)
                    && phase_set.contains(&dep_idx)
                {
                    dep_indices.insert(dep_idx);
                }
            }
            if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                let unresolved_deps = get_resource_dependencies(unresolved);
                for dep_binding in &unresolved_deps {
                    if let Some(&dep_idx) = binding_to_idx.get(dep_binding)
                        && phase_set.contains(&dep_idx)
                    {
                        dep_indices.insert(dep_idx);
                    }
                }
            }
        }
        deps_of.insert(idx, dep_indices);
    }
    deps_of
}

/// Result of a phased effect operation within a single phase.
enum PhaseEffectResult {
    /// Phase 1: Non-replace effect succeeded (Create/Update)
    Success {
        state: Option<State>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
    },
    /// Phase 1: Non-replace effect failed
    Failure {
        binding: Option<String>,
        refresh: Option<(ResourceId, String)>,
    },
    /// Phase 1: Delete succeeded
    Deleted { resource_id: ResourceId },
    /// Phase 2: CBD create succeeded
    CbdCreateSuccess {
        idx: usize,
        state: State,
        cascade_states: Vec<(ResourceId, State, HashMap<String, Value>)>,
    },
    /// Phase 2: CBD create failed
    CbdCreateFailure {
        binding: Option<String>,
        refreshes: Vec<(ResourceId, String)>,
    },
    /// Phase 3: Replace delete succeeded
    ReplaceDeleteSuccess,
    /// Phase 3: Replace delete failed
    ReplaceDeleteFailure {
        binding: Option<String>,
        refresh: Option<(ResourceId, String)>,
        cbd_refresh: Option<(ResourceId, String)>,
    },
    /// Phase 4: Non-CBD create succeeded
    NonCbdCreateSuccess {
        state: State,
        resource_id: ResourceId,
        resolved_attrs: HashMap<String, Value>,
    },
    /// Phase 4: Non-CBD create failed
    NonCbdCreateFailure { binding: Option<String> },
    /// Phase 4: CBD finalization succeeded
    CbdFinalizeSuccess {
        state: State,
        resource_id: ResourceId,
        permanent_overrides: Option<(ResourceId, HashMap<String, String>)>,
    },
    /// Phase 4: CBD finalization failed (rename failed)
    CbdFinalizeFailed {
        state: State,
        resource_id: ResourceId,
        binding: Option<String>,
    },
}

/// Execute effects with dependency-aware ordering for interdependent Replace effects.
///
/// Decomposes Replace effects into phases:
/// 1. Non-Replace effects — independent effects run concurrently
/// 2. CBD creates in forward dependency order — independent creates run concurrently
/// 3. All deletes in reverse dependency order — independent deletes run concurrently
/// 4. Non-CBD creates and CBD finalization — independent creates run concurrently
async fn execute_effects_phased(
    provider: &dyn Provider,
    input: &mut ExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
) -> ExecutionResult {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let mut permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();

    let total = count_actionable_effects(input.plan.effects());
    let completed = AtomicUsize::new(0);

    let effects = input.plan.effects();
    let replace_bindings = collect_replace_bindings(effects);
    let sorted_indices = topological_sort_replaces(effects, &replace_bindings);

    // -----------------------------------------------------------------------
    // Phase 1: Non-Replace effects with parallel execution
    // -----------------------------------------------------------------------
    {
        let phase1_indices: Vec<usize> = (0..effects.len())
            .filter(|&idx| !matches!(&effects[idx], Effect::Replace { .. } | Effect::Read { .. }))
            .collect();

        let deps_of =
            build_phase_dependency_map(effects, &phase1_indices, input.unresolved_resources);
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        let mut in_flight = FuturesUnordered::new();

        loop {
            let mut newly_ready: Vec<usize> = Vec::new();
            for &idx in &phase1_indices {
                if dispatched.contains(&idx) {
                    continue;
                }
                let deps = &deps_of[&idx];
                if deps.iter().all(|d| completed_indices.contains(d)) {
                    newly_ready.push(idx);
                }
            }
            newly_ready.sort();

            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = &effects[idx];

                if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let reason = format!("dependency '{}' failed", failed_dep);
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect,
                        reason: &reason,
                        progress: ProgressInfo {
                            completed: c,
                            total,
                        },
                    });
                    skip_count += 1;
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                    completed_indices.insert(idx);
                    continue;
                }

                let binding_snapshot = input.binding_map.clone();
                let unresolved = &input.unresolved_resources;
                let completed_ref = &completed;

                in_flight.push(async move {
                    let result = match effect {
                        Effect::Create(resource) => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted { effect });
                            let resolved = resolve_resource(resource, &binding_snapshot);
                            match provider.create(&resolved).await {
                                Ok(state) => {
                                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                                        effect,
                                        state: Some(&state),
                                        duration: started.elapsed(),
                                        progress: ProgressInfo {
                                            completed: c,
                                            total,
                                        },
                                    });
                                    PhaseEffectResult::Success {
                                        state: Some(state),
                                        resource_id: resource.id.clone(),
                                        resolved_attrs: Some(resolved.attributes),
                                    }
                                }
                                Err(e) => {
                                    let error_str = e.to_string();
                                    observer.on_event(&ExecutionEvent::EffectFailed {
                                        effect,
                                        error: &error_str,
                                        duration: started.elapsed(),
                                        progress: ProgressInfo {
                                            completed: c,
                                            total,
                                        },
                                    });
                                    PhaseEffectResult::Failure {
                                        binding: effect.binding_name(),
                                        refresh: None,
                                    }
                                }
                            }
                        }
                        Effect::Update { id, from, to, .. } => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted { effect });
                            let resolve_source = unresolved.get(id).unwrap_or(to);
                            let resolved_to =
                                resolve_resource_with_source(to, resolve_source, &binding_snapshot);
                            let identifier = from.identifier.as_deref().unwrap_or("");
                            match provider.update(id, identifier, from, &resolved_to).await {
                                Ok(state) => {
                                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                                        effect,
                                        state: Some(&state),
                                        duration: started.elapsed(),
                                        progress: ProgressInfo {
                                            completed: c,
                                            total,
                                        },
                                    });
                                    PhaseEffectResult::Success {
                                        state: Some(state),
                                        resource_id: id.clone(),
                                        resolved_attrs: Some(resolved_to.attributes),
                                    }
                                }
                                Err(e) => {
                                    let error_str = e.to_string();
                                    observer.on_event(&ExecutionEvent::EffectFailed {
                                        effect,
                                        error: &error_str,
                                        duration: started.elapsed(),
                                        progress: ProgressInfo {
                                            completed: c,
                                            total,
                                        },
                                    });
                                    PhaseEffectResult::Failure {
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
                        } => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted { effect });
                            match provider.delete(id, identifier, lifecycle).await {
                                Ok(()) => {
                                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                                        effect,
                                        state: None,
                                        duration: started.elapsed(),
                                        progress: ProgressInfo {
                                            completed: c,
                                            total,
                                        },
                                    });
                                    PhaseEffectResult::Deleted {
                                        resource_id: id.clone(),
                                    }
                                }
                                Err(e) => {
                                    let error_str = e.to_string();
                                    observer.on_event(&ExecutionEvent::EffectFailed {
                                        effect,
                                        error: &error_str,
                                        duration: started.elapsed(),
                                        progress: ProgressInfo {
                                            completed: c,
                                            total,
                                        },
                                    });
                                    PhaseEffectResult::Failure {
                                        binding: None,
                                        refresh: Some((id.clone(), identifier.clone())),
                                    }
                                }
                            }
                        }
                        _ => unreachable!(),
                    };
                    (idx, result)
                });
            }

            if in_flight.is_empty() {
                break;
            }

            let (finished_idx, result) = in_flight.next().await.unwrap();
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::Success {
                    state,
                    resource_id,
                    resolved_attrs,
                } => {
                    success_count += 1;
                    if let Some(state) = &state {
                        applied_states.insert(resource_id, state.clone());
                        if let Some(attrs) = &resolved_attrs {
                            update_binding_map(&mut input.binding_map, attrs, state);
                        }
                    }
                }
                PhaseEffectResult::Failure {
                    binding, refresh, ..
                } => {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                    if let Some((id, identifier)) = refresh
                        && !identifier.is_empty()
                    {
                        pending_refreshes.insert(id, identifier);
                    }
                }
                PhaseEffectResult::Deleted { resource_id, .. } => {
                    success_count += 1;
                    successfully_deleted.insert(resource_id);
                }
                _ => unreachable!(),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: CBD creates with parallel execution (forward dependency order)
    // -----------------------------------------------------------------------
    let mut cbd_create_states: HashMap<usize, State> = HashMap::new();
    let mut replace_start_times: HashMap<usize, Instant> = HashMap::new();

    // Assign progress numbers to all Replace effects upfront.
    // We use AtomicUsize so we just advance for each replace effect.
    // But to maintain consistent total progress, advance the counter for all replaces.
    let replace_progress_base = completed.load(Ordering::Relaxed);
    let mut replace_progress: HashMap<usize, ProgressInfo> = HashMap::new();
    for (i, &idx) in sorted_indices.iter().enumerate() {
        let c = replace_progress_base + i + 1;
        replace_progress.insert(
            idx,
            ProgressInfo {
                completed: c,
                total,
            },
        );
    }
    // Advance the counter past all replace effects
    completed.store(
        replace_progress_base + sorted_indices.len(),
        Ordering::Relaxed,
    );

    {
        let cbd_indices: Vec<usize> = sorted_indices
            .iter()
            .copied()
            .filter(|&idx| {
                matches!(&effects[idx], Effect::Replace { lifecycle, .. } if lifecycle.create_before_destroy)
            })
            .collect();

        let deps_of = build_phase_dependency_map(effects, &cbd_indices, input.unresolved_resources);
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        let mut in_flight = FuturesUnordered::new();

        loop {
            let mut newly_ready: Vec<usize> = Vec::new();
            for &idx in &cbd_indices {
                if dispatched.contains(&idx) {
                    continue;
                }
                let deps = &deps_of[&idx];
                if deps.iter().all(|d| completed_indices.contains(d)) {
                    newly_ready.push(idx);
                }
            }
            newly_ready.sort();

            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = &effects[idx];
                let progress = replace_progress[&idx];

                if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                    let reason = format!("dependency '{}' failed", failed_dep);
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect,
                        reason: &reason,
                        progress,
                    });
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                    completed_indices.insert(idx);
                    continue;
                }

                let binding_snapshot = input.binding_map.clone();
                let unresolved = &input.unresolved_resources;

                in_flight.push(async move {
                    if let Effect::Replace {
                        to,
                        cascading_updates,
                        ..
                    } = effect
                    {
                        let started = Instant::now();
                        observer.on_event(&ExecutionEvent::EffectStarted { effect });

                        let resolve_source = unresolved.get(&to.id).unwrap_or(to);
                        let resolved =
                            resolve_resource_with_source(to, resolve_source, &binding_snapshot);

                        match provider.create(&resolved).await {
                            Ok(state) => {
                                let mut local_binding_map = binding_snapshot.clone();
                                update_binding_map(
                                    &mut local_binding_map,
                                    &resolved.attributes,
                                    &state,
                                );

                                let mut cascade_failed = false;
                                let mut refreshes = Vec::new();
                                let mut cascade_states = Vec::new();
                                for cascade in cascading_updates {
                                    let resolved_to =
                                        resolve_resource(&cascade.to, &local_binding_map);
                                    let cascade_identifier =
                                        cascade.from.identifier.as_deref().unwrap_or("");
                                    match provider
                                        .update(
                                            &cascade.id,
                                            cascade_identifier,
                                            &cascade.from,
                                            &resolved_to,
                                        )
                                        .await
                                    {
                                        Ok(cascade_state) => {
                                            observer.on_event(
                                                &ExecutionEvent::CascadeUpdateSucceeded {
                                                    id: &cascade.id,
                                                },
                                            );
                                            update_binding_map(
                                                &mut local_binding_map,
                                                &resolved_to.attributes,
                                                &cascade_state,
                                            );
                                            cascade_states.push((
                                                cascade.id.clone(),
                                                cascade_state,
                                                resolved_to.attributes,
                                            ));
                                        }
                                        Err(e) => {
                                            let error_str = e.to_string();
                                            observer.on_event(
                                                &ExecutionEvent::CascadeUpdateFailed {
                                                    id: &cascade.id,
                                                    error: &error_str,
                                                },
                                            );
                                            refreshes.push((
                                                cascade.id.clone(),
                                                cascade_identifier.to_string(),
                                            ));
                                            cascade_failed = true;
                                            break;
                                        }
                                    }
                                }

                                if cascade_failed {
                                    refreshes.push((
                                        to.id.clone(),
                                        state.identifier.clone().unwrap_or_default(),
                                    ));
                                    (
                                        idx,
                                        started,
                                        PhaseEffectResult::CbdCreateFailure {
                                            binding: effect.binding_name(),
                                            refreshes,
                                        },
                                    )
                                } else {
                                    (
                                        idx,
                                        started,
                                        PhaseEffectResult::CbdCreateSuccess {
                                            idx,
                                            state,
                                            cascade_states,
                                        },
                                    )
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
                                (
                                    idx,
                                    started,
                                    PhaseEffectResult::CbdCreateFailure {
                                        binding: effect.binding_name(),
                                        refreshes: Vec::new(),
                                    },
                                )
                            }
                        }
                    } else {
                        unreachable!()
                    }
                });
            }

            if in_flight.is_empty() {
                break;
            }

            let (finished_idx, started, result) = in_flight.next().await.unwrap();
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::CbdCreateSuccess {
                    idx,
                    state,
                    cascade_states,
                } => {
                    let effect = &effects[idx];
                    if let Effect::Replace { to, .. } = effect {
                        update_binding_map(&mut input.binding_map, &to.attributes, &state);
                    }
                    for (cascade_id, cascade_state, cascade_attrs) in cascade_states {
                        applied_states.insert(cascade_id, cascade_state.clone());
                        update_binding_map(&mut input.binding_map, &cascade_attrs, &cascade_state);
                    }
                    replace_start_times.insert(idx, started);
                    cbd_create_states.insert(idx, state);
                }
                PhaseEffectResult::CbdCreateFailure {
                    binding, refreshes, ..
                } => {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                    for (id, identifier) in refreshes {
                        queue_state_refresh(&mut pending_refreshes, &id, Some(&identifier));
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 3: All deletes with parallel execution (reverse dependency order)
    // -----------------------------------------------------------------------
    {
        // Collect indices for deletes that should execute: all Replace effects
        // that haven't been failed/skipped. For CBD, skip if create phase failed.
        let delete_indices: Vec<usize> = sorted_indices
            .iter()
            .rev()
            .copied()
            .filter(|&idx| {
                let effect = &effects[idx];
                if let Effect::Replace { lifecycle, .. } = effect {
                    // Skip if dependency failed
                    if find_failed_dependency(effect, &failed_bindings).is_some() {
                        return false;
                    }
                    // For CBD, skip if create didn't succeed
                    if lifecycle.create_before_destroy && !cbd_create_states.contains_key(&idx) {
                        return false;
                    }
                    true
                } else {
                    false
                }
            })
            .collect();

        // For phase 3, dependencies are reversed: dependents should delete before parents.
        // Build a reverse dependency map for the delete phase.
        let phase_set: HashSet<usize> = delete_indices.iter().copied().collect();
        let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
        for &idx in &delete_indices {
            if let Some(binding) = effects[idx].binding_name() {
                binding_to_idx.insert(binding, idx);
            }
        }
        let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
        for &idx in &delete_indices {
            let effect = &effects[idx];
            let mut dep_indices = HashSet::new();
            if let Some(resource) = effect.resource() {
                let dep_bindings = get_resource_dependencies(resource);
                for dep_binding in &dep_bindings {
                    if let Some(&dep_idx) = binding_to_idx.get(dep_binding)
                        && phase_set.contains(&dep_idx)
                    {
                        dep_indices.insert(dep_idx);
                    }
                }
                if let Some(unresolved) = input.unresolved_resources.get(effect.resource_id()) {
                    let unresolved_deps = get_resource_dependencies(unresolved);
                    for dep_binding in &unresolved_deps {
                        if let Some(&dep_idx) = binding_to_idx.get(dep_binding)
                            && phase_set.contains(&dep_idx)
                        {
                            dep_indices.insert(dep_idx);
                        }
                    }
                }
            }
            deps_of.insert(idx, dep_indices);
        }

        // For reverse order: swap the dependency direction.
        // In forward order, parent has no deps, child depends on parent.
        // In reverse (delete) order, child has no deps, parent depends on child.
        let mut reverse_deps: HashMap<usize, HashSet<usize>> = HashMap::new();
        for &idx in &delete_indices {
            reverse_deps.insert(idx, HashSet::new());
        }
        for (&idx, deps) in &deps_of {
            for &dep_idx in deps {
                // idx depends on dep_idx in forward order
                // So dep_idx should wait for idx in reverse order
                reverse_deps.entry(dep_idx).or_default().insert(idx);
            }
        }

        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        let mut in_flight = FuturesUnordered::new();

        loop {
            let mut newly_ready: Vec<usize> = Vec::new();
            for &idx in &delete_indices {
                if dispatched.contains(&idx) {
                    continue;
                }
                let deps = &reverse_deps[&idx];
                if deps.iter().all(|d| completed_indices.contains(d)) {
                    newly_ready.push(idx);
                }
            }
            newly_ready.sort();

            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = &effects[idx];
                let progress = replace_progress[&idx];
                let is_cbd = matches!(effect, Effect::Replace { lifecycle, .. } if lifecycle.create_before_destroy);

                // For non-CBD replaces, this is where the effect starts
                if !is_cbd {
                    let started = Instant::now();
                    replace_start_times.insert(idx, started);
                    observer.on_event(&ExecutionEvent::EffectStarted { effect });
                }

                // Pre-compute values needed in the async block
                let effect_started = replace_start_times
                    .get(&idx)
                    .copied()
                    .unwrap_or_else(Instant::now);
                let cbd_refresh_info: Option<(ResourceId, String)> = if is_cbd {
                    if let Effect::Replace { to, .. } = effect {
                        cbd_create_states.get(&idx).map(|state| {
                            (to.id.clone(), state.identifier.clone().unwrap_or_default())
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

                in_flight.push(async move {
                    if let Effect::Replace {
                        id,
                        from,
                        lifecycle,
                        ..
                    } = effect
                    {
                        let identifier = from.identifier.as_deref().unwrap_or("");
                        match provider.delete(id, identifier, lifecycle).await {
                            Ok(()) => (idx, PhaseEffectResult::ReplaceDeleteSuccess),
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::EffectFailed {
                                    effect,
                                    error: &error_str,
                                    duration: effect_started.elapsed(),
                                    progress,
                                });
                                (
                                    idx,
                                    PhaseEffectResult::ReplaceDeleteFailure {
                                        binding: effect.binding_name(),
                                        refresh: Some((id.clone(), identifier.to_string())),
                                        cbd_refresh: cbd_refresh_info,
                                    },
                                )
                            }
                        }
                    } else {
                        unreachable!()
                    }
                });
            }

            if in_flight.is_empty() {
                break;
            }

            let (finished_idx, result) = in_flight.next().await.unwrap();
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::ReplaceDeleteSuccess => {
                    // Delete succeeded, will be finalized in phase 4
                }
                PhaseEffectResult::ReplaceDeleteFailure {
                    binding,
                    refresh,
                    cbd_refresh,
                } => {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                    if let Some((id, identifier)) = refresh {
                        queue_state_refresh(&mut pending_refreshes, &id, Some(&identifier));
                    }
                    if let Some((id, identifier)) = cbd_refresh {
                        queue_state_refresh(&mut pending_refreshes, &id, Some(&identifier));
                    }
                    // Remove from cbd_create_states since delete failed
                    cbd_create_states.remove(&finished_idx);
                }
                _ => unreachable!(),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 4: Non-CBD creates and CBD finalization with parallel execution
    // -----------------------------------------------------------------------
    {
        let phase4_indices: Vec<usize> = sorted_indices.clone();

        let deps_of =
            build_phase_dependency_map(effects, &phase4_indices, input.unresolved_resources);
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        type PhaseFuture<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (usize, PhaseEffectResult)> + 'a>>;
        let mut in_flight: FuturesUnordered<PhaseFuture<'_>> = FuturesUnordered::new();

        loop {
            let mut newly_ready: Vec<usize> = Vec::new();
            for &idx in &phase4_indices {
                if dispatched.contains(&idx) {
                    continue;
                }
                let deps = &deps_of[&idx];
                if deps.iter().all(|d| completed_indices.contains(d)) {
                    newly_ready.push(idx);
                }
            }
            newly_ready.sort();

            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = &effects[idx];
                let progress = replace_progress[&idx];

                if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                    let reason = format!("dependency '{}' failed", failed_dep);
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect,
                        reason: &reason,
                        progress,
                    });
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                    completed_indices.insert(idx);
                    continue;
                }

                let binding_snapshot = input.binding_map.clone();
                let unresolved = &input.unresolved_resources;

                if let Effect::Replace {
                    id,
                    to,
                    lifecycle,
                    temporary_name,
                    ..
                } = effect
                {
                    let effect_started = replace_start_times
                        .get(&idx)
                        .copied()
                        .unwrap_or_else(Instant::now);

                    if lifecycle.create_before_destroy {
                        // CBD finalization: skip if create phase failed
                        let Some(state) = cbd_create_states.remove(&idx) else {
                            completed_indices.insert(idx);
                            continue;
                        };
                        let id = id.clone();
                        let to = to.clone();
                        let temporary_name = temporary_name.clone();

                        in_flight.push(Box::pin(async move {
                            let started = effect_started;

                            if let Some(temp) = temporary_name.as_ref()
                                && temp.can_rename
                            {
                                let new_identifier = state.identifier.as_deref().unwrap_or("");
                                let mut rename_to = to.clone();
                                rename_to.attributes.insert(
                                    temp.attribute.clone(),
                                    Value::String(temp.original_value.clone()),
                                );
                                match provider
                                    .update(&id, new_identifier, &state, &rename_to)
                                    .await
                                {
                                    Ok(renamed_state) => {
                                        observer.on_event(&ExecutionEvent::RenameSucceeded {
                                            id: &id,
                                            from: &temp.temporary_value,
                                            to: &temp.original_value,
                                        });
                                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                                            effect,
                                            state: None,
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::CbdFinalizeSuccess {
                                                state: renamed_state,
                                                resource_id: to.id.clone(),
                                                permanent_overrides: None,
                                            },
                                        )
                                    }
                                    Err(e) => {
                                        let error_str = e.to_string();
                                        observer.on_event(&ExecutionEvent::RenameFailed {
                                            id: &id,
                                            error: &error_str,
                                        });
                                        observer.on_event(&ExecutionEvent::EffectFailed {
                                            effect,
                                            error: "rename failed",
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::CbdFinalizeFailed {
                                                state,
                                                resource_id: to.id.clone(),
                                                binding: effect.binding_name(),
                                            },
                                        )
                                    }
                                }
                            } else {
                                // No rename needed or can_rename=false
                                let permanent_overrides =
                                    temporary_name.as_ref().and_then(|temp| {
                                        if !temp.can_rename {
                                            let mut overrides = HashMap::new();
                                            overrides.insert(
                                                temp.attribute.clone(),
                                                temp.temporary_value.clone(),
                                            );
                                            Some((to.id.clone(), overrides))
                                        } else {
                                            None
                                        }
                                    });
                                observer.on_event(&ExecutionEvent::EffectSucceeded {
                                    effect,
                                    state: None,
                                    duration: started.elapsed(),
                                    progress,
                                });
                                (
                                    idx,
                                    PhaseEffectResult::CbdFinalizeSuccess {
                                        state,
                                        resource_id: to.id.clone(),
                                        permanent_overrides,
                                    },
                                )
                            }
                        }));
                    } else {
                        // Non-CBD: skip if own delete failed
                        if let Some(binding) = effect.binding_name()
                            && failed_bindings.contains(&binding)
                        {
                            completed_indices.insert(idx);
                            continue;
                        }

                        // Non-CBD: create new resource
                        in_flight.push(Box::pin(async move {
                            if let Effect::Replace { to, .. } = effect {
                                let started = effect_started;
                                let resolve_source = unresolved.get(&to.id).unwrap_or(to);
                                let resolved = resolve_resource_with_source(
                                    to,
                                    resolve_source,
                                    &binding_snapshot,
                                );

                                match provider.create(&resolved).await {
                                    Ok(state) => {
                                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                                            effect,
                                            state: Some(&state),
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::NonCbdCreateSuccess {
                                                state,
                                                resource_id: to.id.clone(),
                                                resolved_attrs: resolved.attributes,
                                            },
                                        )
                                    }
                                    Err(e) => {
                                        let error_str = e.to_string();
                                        observer.on_event(&ExecutionEvent::EffectFailed {
                                            effect,
                                            error: &error_str,
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::NonCbdCreateFailure {
                                                binding: effect.binding_name(),
                                            },
                                        )
                                    }
                                }
                            } else {
                                unreachable!()
                            }
                        }));
                    }
                }
            }

            if in_flight.is_empty() {
                break;
            }

            let (finished_idx, result) = in_flight.next().await.unwrap();
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::CbdFinalizeSuccess {
                    state,
                    resource_id,
                    permanent_overrides,
                } => {
                    success_count += 1;
                    applied_states.insert(resource_id, state);
                    if let Some((id, overrides)) = permanent_overrides {
                        permanent_name_overrides.insert(id, overrides);
                    }
                }
                PhaseEffectResult::CbdFinalizeFailed {
                    state,
                    resource_id,
                    binding,
                } => {
                    failure_count += 1;
                    applied_states.insert(resource_id, state);
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                }
                PhaseEffectResult::NonCbdCreateSuccess {
                    state,
                    resource_id,
                    resolved_attrs,
                } => {
                    success_count += 1;
                    applied_states.insert(resource_id, state.clone());
                    update_binding_map(&mut input.binding_map, &resolved_attrs, &state);
                }
                PhaseEffectResult::NonCbdCreateFailure { binding } => {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                }
                _ => unreachable!(),
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

    struct MockObserver {
        events: Mutex<Vec<String>>,
    }

    impl MockObserver {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ExecutionObserver for MockObserver {
        fn on_event(&self, event: &ExecutionEvent) {
            let msg = match event {
                ExecutionEvent::Waiting {
                    effect,
                    pending_dependencies,
                } => {
                    format!(
                        "waiting:{}:[{}]",
                        effect.resource_id(),
                        pending_dependencies.join(",")
                    )
                }
                ExecutionEvent::EffectStarted { effect } => {
                    format!("started:{}", effect.resource_id())
                }
                ExecutionEvent::EffectSucceeded { effect, .. } => {
                    format!("succeeded:{}", effect.resource_id())
                }
                ExecutionEvent::EffectFailed { effect, error, .. } => {
                    format!("failed:{}:{}", effect.resource_id(), error)
                }
                ExecutionEvent::EffectSkipped { effect, reason, .. } => {
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
            self.events.lock().unwrap().push(msg);
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
                    field_path: vec![],
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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 1);
        assert_eq!(result.failure_count, 0);
        assert!(
            observer
                .events()
                .iter()
                .any(|e| e.starts_with("succeeded:"))
        );
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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.failure_count, 1);
        assert_eq!(result.skip_count, 1);
        assert!(observer.events().iter().any(|e| e.contains("failed:")));
        assert!(
            observer
                .events()
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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

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

        let observer = MockObserver::new();
        execute_plan(&provider, input, &observer).await;

        let events = observer.events();
        assert_eq!(events.len(), 2);
        assert!(events[0].starts_with("started:"));
        assert!(events[1].starts_with("succeeded:"));
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

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 0);
        assert!(provider.calls().is_empty());
    }

    #[tokio::test]
    async fn test_independent_effects_run_in_parallel() {
        // vpc has no deps, subnet_a and subnet_b both depend on vpc.
        // Expected: vpc runs first (level 0), then subnet_a and subnet_b
        // run concurrently (level 1).
        let provider = MockProvider::new();
        let vpc = make_resource("vpc", &[]);
        let subnet_a = make_resource("subnet_a", &["vpc"]);
        let subnet_b = make_resource("subnet_b", &["vpc"]);
        let vpc_id = vpc.id.clone();
        let subnet_a_id = subnet_a.id.clone();
        let subnet_b_id = subnet_b.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(subnet_a));
        plan.add(Effect::Create(subnet_b));

        provider.push_create(Ok(ok_state(&vpc_id)));
        provider.push_create(Ok(ok_state(&subnet_a_id)));
        provider.push_create(Ok(ok_state(&subnet_b_id)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 3);
        assert_eq!(result.failure_count, 0);

        // vpc should be created first (level 0), before either subnet
        let calls = provider.calls();
        assert_eq!(calls[0], ("create".to_string(), vpc_id.to_string()));

        // Both subnets should be created (level 1), order may vary
        let remaining: HashSet<String> = calls[1..].iter().map(|(_, id)| id.clone()).collect();
        assert!(remaining.contains(&subnet_a_id.to_string()));
        assert!(remaining.contains(&subnet_b_id.to_string()));
    }

    #[tokio::test]
    async fn test_parallel_failure_skips_dependents() {
        // vpc (level 0), subnet_a depends on vpc, subnet_b depends on vpc.
        // vpc succeeds. subnet_a fails. subnet_c depends on subnet_a => skipped.
        let provider = MockProvider::new();
        let vpc = make_resource("vpc", &[]);
        let subnet_a = make_resource("subnet_a", &["vpc"]);
        let subnet_b = make_resource("subnet_b", &["vpc"]);
        let subnet_c = make_resource("subnet_c", &["subnet_a"]);
        let vpc_id = vpc.id.clone();
        let _subnet_a_id = subnet_a.id.clone();
        let subnet_b_id = subnet_b.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(subnet_a));
        plan.add(Effect::Create(subnet_b));
        plan.add(Effect::Create(subnet_c));

        provider.push_create(Ok(ok_state(&vpc_id)));
        // subnet_a fails, subnet_b succeeds
        provider.push_create(Err(ProviderError::new("create failed")));
        provider.push_create(Ok(ok_state(&subnet_b_id)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        // vpc + subnet_b succeed, subnet_a fails, subnet_c skipped
        assert_eq!(result.success_count, 2);
        assert_eq!(result.failure_count, 1);
        assert_eq!(result.skip_count, 1);

        // Verify subnet_c was skipped due to subnet_a failure
        assert!(
            observer
                .events()
                .iter()
                .any(|e| e.contains("skipped:") && e.contains("dependency 'subnet_a' failed"))
        );
    }

    #[tokio::test]
    async fn test_dependency_levels_sequential_chain() {
        // a -> b -> c: should be 3 levels, executed sequentially
        let provider = MockProvider::new();
        let a = make_resource("a", &[]);
        let b = make_resource("b", &["a"]);
        let c = make_resource("c", &["b"]);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        let c_id = c.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(a));
        plan.add(Effect::Create(b));
        plan.add(Effect::Create(c));

        provider.push_create(Ok(ok_state(&a_id)));
        provider.push_create(Ok(ok_state(&b_id)));
        provider.push_create(Ok(ok_state(&c_id)));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 3);

        // Calls should be in order: a, b, c
        let calls = provider.calls();
        assert_eq!(calls[0], ("create".to_string(), a_id.to_string()));
        assert_eq!(calls[1], ("create".to_string(), b_id.to_string()));
        assert_eq!(calls[2], ("create".to_string(), c_id.to_string()));
    }

    #[test]
    fn test_build_dependency_levels() {
        // a (no deps), b depends on a, c depends on a, d depends on b and c
        let a = make_resource("a", &[]);
        let b = make_resource("b", &["a"]);
        let c = make_resource("c", &["a"]);
        let d = make_resource("d", &["b", "c"]);

        let mut plan = Plan::new();
        plan.add(Effect::Create(a));
        plan.add(Effect::Create(b));
        plan.add(Effect::Create(c));
        plan.add(Effect::Create(d));

        let levels = build_dependency_levels(plan.effects(), &HashMap::new());

        // Level 0: a (index 0)
        // Level 1: b (index 1), c (index 2)
        // Level 2: d (index 3)
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec![0]);
        assert_eq!(levels[1], vec![1, 2]);
        assert_eq!(levels[2], vec![3]);
    }

    /// Regression test for #1078: route must depend on tgw_attach even when
    /// resolve_refs_with_state partially resolves `tgw_attach.transit_gateway_id`
    /// to `ResourceRef { binding: "tgw", attr: "id" }`.
    ///
    /// Before the fix, the route and tgw_attach were placed at the same dependency
    /// level and executed in parallel, causing an AWS API error.
    #[test]
    fn test_build_dependency_levels_transitive_ref_preserves_direct_dep() {
        use crate::plan::Plan;

        // Simulate the resources as they appear in the effects after
        // resolve_refs_with_state: ResourceRef values are partially resolved,
        // but _dependency_bindings records the original direct dependencies.

        // tgw_attach depends on tgw, vpc, subnet
        let mut tgw_attach = Resource::new("ec2.transit_gateway_attachment", "tgw_attach");
        tgw_attach.attributes.insert(
            "_binding".to_string(),
            Value::String("tgw_attach".to_string()),
        );
        tgw_attach.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![
                Value::String("tgw".to_string()),
                Value::String("vpc".to_string()),
                Value::String("subnet".to_string()),
            ]),
        );

        // route depends on rt and tgw_attach (but after partial resolution,
        // transit_gateway_id points to ResourceRef { binding: "tgw" })
        let mut route = Resource::new("ec2.route", "my-route");
        route.attributes.insert(
            "transit_gateway_id".to_string(),
            Value::ResourceRef {
                binding_name: "tgw".to_string(),
                attribute_name: "id".to_string(),
                field_path: vec![],
            },
        );
        route.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![
                Value::String("rt".to_string()),
                Value::String("tgw_attach".to_string()),
            ]),
        );

        // Other resources
        let mut vpc = Resource::new("ec2.vpc", "vpc");
        vpc.attributes
            .insert("_binding".to_string(), Value::String("vpc".to_string()));

        let mut tgw = Resource::new("ec2.transit_gateway", "tgw");
        tgw.attributes
            .insert("_binding".to_string(), Value::String("tgw".to_string()));

        let mut subnet = Resource::new("ec2.subnet", "subnet");
        subnet
            .attributes
            .insert("_binding".to_string(), Value::String("subnet".to_string()));
        subnet.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![Value::String("vpc".to_string())]),
        );

        let mut rt = Resource::new("ec2.route_table", "rt");
        rt.attributes
            .insert("_binding".to_string(), Value::String("rt".to_string()));
        rt.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![Value::String("vpc".to_string())]),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc)); // idx 0
        plan.add(Effect::Create(tgw)); // idx 1
        plan.add(Effect::Create(subnet)); // idx 2
        plan.add(Effect::Create(tgw_attach)); // idx 3
        plan.add(Effect::Create(rt)); // idx 4
        plan.add(Effect::Create(route)); // idx 5

        let levels = build_dependency_levels(plan.effects(), &HashMap::new());

        // Find the level of tgw_attach (idx 3) and route (idx 5)
        let tgw_attach_level = levels.iter().position(|group| group.contains(&3)).unwrap();
        let route_level = levels.iter().position(|group| group.contains(&5)).unwrap();

        assert!(
            route_level > tgw_attach_level,
            "route (level {}) must be at a higher level than tgw_attach (level {}). levels: {:?}",
            route_level,
            tgw_attach_level,
            levels
        );
    }

    /// Verify fine-grained scheduling: effect C (depends on A) starts before
    /// effect B (independent, slow) completes.
    ///
    /// Setup:
    ///   A (no deps, fast), B (no deps, slow), C (depends on A, fast)
    ///
    /// With level-based execution:
    ///   Level 0: A and B run concurrently, wait for both.
    ///   Level 1: C starts after B finishes (~100ms total).
    ///
    /// With fine-grained scheduling:
    ///   A and B start concurrently. A finishes quickly (~5ms).
    ///   C starts immediately (A is done), while B is still running.
    ///   C should start (and finish) before B completes.
    #[tokio::test]
    async fn test_fine_grained_scheduling_starts_dependent_before_slow_peer_completes() {
        use std::time::Duration;

        // A provider that delays certain resources
        struct DelayedProvider {
            delays: HashMap<String, Duration>,
            call_log: Arc<Mutex<Vec<(String, String, Instant)>>>,
        }

        impl Provider for DelayedProvider {
            fn name(&self) -> &'static str {
                "delayed"
            }

            fn read(
                &self,
                _id: &ResourceId,
                _identifier: Option<&str>,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                Box::pin(async { Err(ProviderError::new("not implemented")) })
            }

            fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
                let id = resource.id.clone();
                let name = resource.id.name.clone();
                let delay = self.delays.get(&name).copied().unwrap_or(Duration::ZERO);
                let log = self.call_log.clone();
                Box::pin(async move {
                    tokio::time::sleep(delay).await;
                    log.lock()
                        .unwrap()
                        .push(("create".to_string(), name, Instant::now()));
                    Ok(State::existing(id, HashMap::new()).with_identifier("id-123"))
                })
            }

            fn update(
                &self,
                _id: &ResourceId,
                _identifier: &str,
                _from: &State,
                _to: &Resource,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                Box::pin(async { Err(ProviderError::new("not implemented")) })
            }

            fn delete(
                &self,
                _id: &ResourceId,
                _identifier: &str,
                _lifecycle: &LifecycleConfig,
            ) -> BoxFuture<'_, ProviderResult<()>> {
                Box::pin(async { Err(ProviderError::new("not implemented")) })
            }
        }

        let mut delays = HashMap::new();
        delays.insert("a".to_string(), Duration::from_millis(5));
        delays.insert("b".to_string(), Duration::from_millis(200));
        delays.insert("c".to_string(), Duration::from_millis(5));

        let call_log = Arc::new(Mutex::new(Vec::new()));
        let provider = DelayedProvider {
            delays,
            call_log: call_log.clone(),
        };

        let a = make_resource("a", &[]);
        let b = make_resource("b", &[]);
        let c = make_resource("c", &["a"]);

        let mut plan = Plan::new();
        plan.add(Effect::Create(a));
        plan.add(Effect::Create(b));
        plan.add(Effect::Create(c));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 3);
        assert_eq!(result.failure_count, 0);

        // Verify C completed before B.
        // With fine-grained scheduling, C starts right after A completes
        // (while B is still sleeping), so C should finish before B.
        let log = call_log.lock().unwrap();
        let c_time = log.iter().find(|(_, name, _)| name == "c").unwrap().2;
        let b_time = log.iter().find(|(_, name, _)| name == "b").unwrap().2;
        assert!(
            c_time < b_time,
            "C should complete before B with fine-grained scheduling. \
             C completed at {:?}, B completed at {:?}",
            c_time,
            b_time,
        );
    }

    #[tokio::test]
    async fn test_waiting_events_emitted_for_dependent_effects() {
        // Setup: A has no deps, C depends on A.
        // C should get a Waiting event before A completes.
        let a = make_resource("a", &[]);
        let c = make_resource("c", &["a"]);

        let mut plan = Plan::new();
        plan.add(Effect::Create(a));
        plan.add(Effect::Create(c));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let observer = MockObserver::new();
        let provider = MockProvider::new();
        // Push create results for both resources
        let a_id = ResourceId::new("test", "a");
        let c_id = ResourceId::new("test", "c");
        provider.push_create(Ok(
            State::existing(a_id, HashMap::new()).with_identifier("id-a")
        ));
        provider.push_create(Ok(
            State::existing(c_id, HashMap::new()).with_identifier("id-c")
        ));
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 2);

        let events = observer.events.lock().unwrap();
        // C should have a waiting event before it starts
        let waiting_events: Vec<_> = events
            .iter()
            .filter(|e| e.starts_with("waiting:"))
            .collect();
        assert!(
            !waiting_events.is_empty(),
            "Expected at least one waiting event, got events: {:?}",
            *events
        );
        // The waiting event for C should mention dependency "a"
        let c_waiting = waiting_events
            .iter()
            .find(|e| e.contains("test.c"))
            .expect("Expected a waiting event for resource C");
        assert!(
            c_waiting.contains("[a]"),
            "Waiting event should list 'a' as pending dependency, got: {}",
            c_waiting
        );
    }
}
