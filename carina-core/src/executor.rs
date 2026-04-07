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
use crate::resource::{Expr, Resource, ResourceId, State, Value};

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
            for dep in &to.dependency_bindings {
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
            && let Some(ref b) = to.binding
        {
            bindings.insert(b.clone());
        }
    }
    bindings
}

/// Topologically sort Replace effects by dependency order.
/// Returns indices in forward dependency order (parents before dependents).
fn topological_sort_replaces(effects: &[Effect], replace_bindings: &HashSet<String>) -> Vec<usize> {
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    let mut replace_indices: Vec<usize> = Vec::new();

    for (idx, effect) in effects.iter().enumerate() {
        if let Effect::Replace { to, .. } = effect {
            replace_indices.push(idx);
            if let Some(ref b) = to.binding {
                binding_to_idx.insert(b.clone(), idx);
            }
        }
    }

    // Build adjacency: for each replace effect, find which other replace effects it depends on
    let mut deps: HashMap<usize, Vec<usize>> = HashMap::new();
    for &idx in &replace_indices {
        let effect = &effects[idx];
        if let Effect::Replace { to, .. } = effect {
            let dep_indices: Vec<usize> = to
                .dependency_bindings
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
/// Secret values are unwrapped so the provider receives the plain inner value.
fn resolve_resource(
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
fn resolve_resource_with_source(
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
fn update_binding_map(
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

/// Mutable execution state shared across effect processing.
///
/// Groups the counters, result maps, and binding state that are threaded
/// through every call to `process_basic_result`, reducing parameter count.
struct ExecutionState<'a> {
    success_count: &'a mut usize,
    failure_count: &'a mut usize,
    applied_states: &'a mut HashMap<ResourceId, State>,
    failed_bindings: &'a mut HashSet<String>,
    successfully_deleted: &'a mut HashSet<ResourceId>,
    pending_refreshes: &'a mut HashMap<ResourceId, String>,
    binding_map: &'a mut HashMap<String, HashMap<String, Value>>,
}

/// Process a `BasicEffectResult` by updating shared execution state.
///
/// This helper is used by both sequential and phased execution paths to avoid
/// duplicating the result-processing logic for Create/Update/Delete effects.
fn process_basic_result(result: BasicEffectResult, exec: &mut ExecutionState<'_>) {
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

// ---------------------------------------------------------------------------
// Effect execution: sequential path
// ---------------------------------------------------------------------------

/// Count the number of actionable effects (excluding Read and state operations).
fn count_actionable_effects(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| !matches!(e, Effect::Read { .. }) && !e.is_state_operation())
        .count()
}

/// Build dependency levels from effects.
///
/// Groups effects into levels where all effects in a level have their
/// dependencies satisfied by effects in earlier levels. Effects within
/// the same level can be executed concurrently.
///
/// Delegates to `build_dependency_map` for the dependency computation and
/// layers level-grouping logic on top.
#[cfg(test)]
fn build_dependency_levels(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> Vec<Vec<usize>> {
    let deps_of = build_dependency_map(effects, unresolved_resources);

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

/// Result of executing a basic effect (Create, Update, or Delete).
///
/// This is the shared result type used by `execute_basic_effect` to avoid
/// duplicating effect dispatch logic across sequential and phased paths.
enum BasicEffectResult {
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

/// Execute a single Create, Update, or Delete effect.
///
/// This helper encapsulates the shared dispatch logic: increment progress counter,
/// record start time, emit EffectStarted, resolve resource, call provider, and
/// emit EffectSucceeded/EffectFailed. Returns a `BasicEffectResult` that callers
/// map to their path-specific result types.
///
/// Panics if called with a Replace, Read, Import, Remove, or Move effect.
async fn execute_basic_effect<'a>(
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

/// Result of executing a single effect.
enum SingleEffectResult {
    /// Create/Update/Delete completed (wraps BasicEffectResult)
    Basic(BasicEffectResult),
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
    // Build binding -> effect index mapping, plus a secondary lookup for Delete
    // effects that lost their binding (e.g., orphan _binding lost during provider
    // refresh). For let-bound resources, the resource ID name equals the original
    // binding name, so we use it as a fallback key. A Delete with binding: None
    // can't also appear as a Create/Update with the same name in the same plan,
    // so there is no collision risk with binding_to_idx. (#1548)
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    let mut name_to_delete_idx: HashMap<&str, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        if let Some(binding) = effect.binding_name() {
            binding_to_idx.insert(binding, idx);
        }
        if matches!(effect, Effect::Delete { binding: None, .. }) {
            name_to_delete_idx.insert(&effect.resource_id().name, idx);
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

    // For Delete effects, add reverse dependencies: if subnet depends on vpc,
    // the vpc delete must wait for subnet delete (children deleted before parents).
    let mut reverse_deps: Vec<(usize, usize)> = Vec::new();
    for (idx, effect) in effects.iter().enumerate() {
        if let Effect::Delete { dependencies, .. } = effect {
            for dep_binding in dependencies {
                if let Some(&dep_idx) = binding_to_idx
                    .get(dep_binding)
                    .or_else(|| name_to_delete_idx.get(dep_binding.as_str()))
                {
                    reverse_deps.push((dep_idx, idx));
                }
            }
        }
        // For Replace effects (especially CBD), the old resource (from) may have
        // depended on a resource that is now being deleted. The delete of that
        // parent must wait for this Replace to complete first (the old resource
        // is deleted during the Replace's delete phase).
        // Use from.dependency_bindings (recorded in state) for the old dependencies.
        if let Effect::Replace { from, .. } = effect {
            for dep_binding in &from.dependency_bindings {
                if let Some(&dep_idx) = binding_to_idx
                    .get(dep_binding)
                    .or_else(|| name_to_delete_idx.get(dep_binding.as_str()))
                    && matches!(&effects[dep_idx], Effect::Delete { .. })
                {
                    reverse_deps.push((dep_idx, idx));
                }
            }
        }
    }
    for (parent_idx, child_idx) in reverse_deps {
        deps_of.entry(parent_idx).or_default().insert(child_idx);
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
    // All actionable effect indices (excluding Read and state operations)
    let actionable_indices: Vec<usize> = (0..effects.len())
        .filter(|&idx| {
            !matches!(&effects[idx], Effect::Read { .. }) && !effects[idx].is_state_operation()
        })
        .collect();

    // Mark Read and state operation effects as completed (they are no-ops in the executor)
    for (idx, effect) in effects.iter().enumerate() {
        if matches!(effect, Effect::Read { .. }) || effect.is_state_operation() {
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
                    Effect::Create(_) | Effect::Update { .. } | Effect::Delete { .. } => {
                        SingleEffectResult::Basic(
                            execute_basic_effect(
                                effect,
                                provider,
                                &binding_snapshot,
                                unresolved,
                                completed_ref,
                                total,
                                observer,
                            )
                            .await,
                        )
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
                            &ReplaceContext {
                                effect,
                                id,
                                from,
                                to,
                                lifecycle,
                                cascading_updates,
                                temporary_name: temporary_name.as_ref(),
                                binding_map: &binding_snapshot,
                                unresolved,
                                started,
                                progress,
                            },
                            observer,
                        )
                        .await
                    }
                    Effect::Read { .. } => SingleEffectResult::ReadNoOp,
                    // State operations are handled separately during apply
                    Effect::Import { .. } | Effect::Remove { .. } | Effect::Move { .. } => {
                        SingleEffectResult::ReadNoOp
                    }
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
            SingleEffectResult::Basic(basic) => {
                process_basic_result(
                    basic,
                    &mut ExecutionState {
                        success_count: &mut success_count,
                        failure_count: &mut failure_count,
                        applied_states: &mut applied_states,
                        failed_bindings: &mut failed_bindings,
                        successfully_deleted: &mut successfully_deleted,
                        pending_refreshes: &mut pending_refreshes,
                        binding_map: &mut input.binding_map,
                    },
                );
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
                        update_binding_map(
                            &mut input.binding_map,
                            attrs,
                            binding.as_deref(),
                            state,
                        );
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

/// Context for executing a Replace effect in the parallel path.
///
/// Groups the resource data, lifecycle configuration, and execution metadata
/// that are passed to both CBD and DBD replace functions.
struct ReplaceContext<'a> {
    effect: &'a Effect,
    id: &'a ResourceId,
    from: &'a State,
    to: &'a Resource,
    lifecycle: &'a crate::resource::LifecycleConfig,
    cascading_updates: &'a [crate::effect::CascadingUpdate],
    temporary_name: Option<&'a crate::effect::TemporaryName>,
    binding_map: &'a HashMap<String, HashMap<String, Value>>,
    unresolved: &'a HashMap<ResourceId, Resource>,
    started: Instant,
    progress: ProgressInfo,
}

/// Execute a Replace effect, returning a `SingleEffectResult`.
///
/// This handles both CBD and DBD replace within the parallel execution path.
/// It does not mutate shared state directly; instead returns all data needed
/// for the caller to update shared state after the level completes.
async fn execute_replace_parallel(
    provider: &dyn Provider,
    ctx: &ReplaceContext<'_>,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    if ctx.lifecycle.create_before_destroy {
        execute_cbd_replace_parallel(provider, ctx, observer).await
    } else {
        execute_dbd_replace_parallel(provider, ctx, observer).await
    }
}

/// CBD Replace for the parallel execution path.
async fn execute_cbd_replace_parallel(
    provider: &dyn Provider,
    ctx: &ReplaceContext<'_>,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    let resolved = match resolve_resource(ctx.to, ctx.binding_map) {
        Ok(r) => r,
        Err(e) => {
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect: ctx.effect,
                error: &e,
                duration: ctx.started.elapsed(),
                progress: ctx.progress,
            });
            return SingleEffectResult::Basic(BasicEffectResult::Failure {
                binding: ctx.effect.binding_name(),
                refresh: None,
            });
        }
    };
    let mut refreshes = Vec::new();

    match provider.create(&resolved).await {
        Ok(state) => {
            // Build a local binding map update for cascade resolution
            let mut local_binding_map = ctx.binding_map.clone();
            update_binding_map(
                &mut local_binding_map,
                &resolved.resolved_attributes(),
                ctx.to.binding.as_deref(),
                &state,
            );

            // Execute cascading updates
            let mut cascade_failed = false;
            for cascade in ctx.cascading_updates {
                let resolved_to = match resolve_resource(&cascade.to, &local_binding_map) {
                    Ok(r) => r,
                    Err(e) => {
                        observer.on_event(&ExecutionEvent::CascadeUpdateFailed {
                            id: &cascade.id,
                            error: &e,
                        });
                        let cascade_identifier = cascade.from.identifier.as_deref().unwrap_or("");
                        refreshes.push((cascade.id.clone(), cascade_identifier.to_string()));
                        cascade_failed = true;
                        break;
                    }
                };
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
                            &resolved_to.resolved_attributes(),
                            cascade.to.binding.as_deref(),
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
                refreshes.push((
                    ctx.to.id.clone(),
                    state.identifier.clone().unwrap_or_default(),
                ));
                return SingleEffectResult::Replace {
                    success: false,
                    state: None,
                    resource_id: ctx.to.id.clone(),
                    resolved_attrs: None,
                    binding: ctx.effect.binding_name(),
                    refreshes,
                    permanent_overrides: None,
                };
            }

            // Delete the old resource
            let identifier = ctx.from.identifier.as_deref().unwrap_or("");
            match provider.delete(ctx.id, identifier, ctx.lifecycle).await {
                Ok(()) => {
                    // Handle rename
                    let mut permanent_overrides = None;
                    let mut final_state = state.clone();
                    let mut rename_failed = false;

                    if let Some(temp) = ctx.temporary_name
                        && temp.can_rename
                    {
                        let new_identifier = state.identifier.as_deref().unwrap_or("");
                        let mut rename_to = ctx.to.clone();
                        rename_to.set_attr(
                            temp.attribute.clone(),
                            Value::String(temp.original_value.clone()),
                        );
                        match provider
                            .update(ctx.id, new_identifier, &state, &rename_to)
                            .await
                        {
                            Ok(renamed_state) => {
                                observer.on_event(&ExecutionEvent::RenameSucceeded {
                                    id: ctx.id,
                                    from: &temp.temporary_value,
                                    to: &temp.original_value,
                                });
                                final_state = renamed_state;
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::RenameFailed {
                                    id: ctx.id,
                                    error: &error_str,
                                });
                                rename_failed = true;
                            }
                        }
                    } else if let Some(temp) = ctx.temporary_name
                        && !temp.can_rename
                    {
                        let mut overrides = HashMap::new();
                        overrides.insert(temp.attribute.clone(), temp.temporary_value.clone());
                        permanent_overrides = Some((ctx.to.id.clone(), overrides));
                    }

                    if rename_failed {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect: ctx.effect,
                            error: "rename failed",
                            duration: ctx.started.elapsed(),
                            progress: ctx.progress,
                        });
                        SingleEffectResult::Replace {
                            success: false,
                            state: Some(final_state),
                            resource_id: ctx.to.id.clone(),
                            resolved_attrs: Some(resolved.resolved_attributes()),
                            binding: ctx.effect.binding_name(),
                            refreshes,

                            permanent_overrides,
                        }
                    } else {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect: ctx.effect,
                            state: None,
                            duration: ctx.started.elapsed(),
                            progress: ctx.progress,
                        });
                        SingleEffectResult::Replace {
                            success: true,
                            state: Some(final_state),
                            resource_id: ctx.to.id.clone(),
                            resolved_attrs: Some(resolved.resolved_attributes()),
                            binding: ctx.to.binding.clone(),
                            refreshes,

                            permanent_overrides,
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect: ctx.effect,
                        error: &error_str,
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    refreshes.push((
                        ctx.to.id.clone(),
                        state.identifier.clone().unwrap_or_default(),
                    ));
                    SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: None,
                        binding: ctx.effect.binding_name(),
                        refreshes,

                        permanent_overrides: None,
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect: ctx.effect,
                error: &error_str,
                duration: ctx.started.elapsed(),
                progress: ctx.progress,
            });
            SingleEffectResult::Replace {
                success: false,
                state: None,
                resource_id: ctx.to.id.clone(),
                resolved_attrs: None,
                binding: ctx.effect.binding_name(),
                refreshes,

                permanent_overrides: None,
            }
        }
    }
}

/// DBD Replace for the parallel execution path.
async fn execute_dbd_replace_parallel(
    provider: &dyn Provider,
    ctx: &ReplaceContext<'_>,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    let identifier = ctx.from.identifier.as_deref().unwrap_or("");
    let mut refreshes = Vec::new();

    match provider.delete(ctx.id, identifier, ctx.lifecycle).await {
        Ok(()) => {
            let resolve_source = ctx.unresolved.get(&ctx.to.id).unwrap_or(ctx.to);
            let resolved =
                match resolve_resource_with_source(ctx.to, resolve_source, ctx.binding_map) {
                    Ok(r) => r,
                    Err(e) => {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect: ctx.effect,
                            error: &e,
                            duration: ctx.started.elapsed(),
                            progress: ctx.progress,
                        });
                        refreshes.push((ctx.to.id.clone(), identifier.to_string()));
                        return SingleEffectResult::Replace {
                            success: false,
                            state: None,
                            resource_id: ctx.to.id.clone(),
                            resolved_attrs: None,
                            binding: ctx.effect.binding_name(),
                            refreshes,
                            permanent_overrides: None,
                        };
                    }
                };
            match provider.create(&resolved).await {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect: ctx.effect,
                        state: Some(&state),
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    SingleEffectResult::Replace {
                        success: true,
                        state: Some(state),
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: Some(resolved.resolved_attributes()),
                        binding: ctx.to.binding.clone(),
                        refreshes,

                        permanent_overrides: None,
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect: ctx.effect,
                        error: &error_str,
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    refreshes.push((ctx.to.id.clone(), identifier.to_string()));
                    SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: None,
                        binding: ctx.effect.binding_name(),
                        refreshes,

                        permanent_overrides: None,
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect: ctx.effect,
                error: &error_str,
                duration: ctx.started.elapsed(),
                progress: ctx.progress,
            });
            refreshes.push((ctx.id.clone(), identifier.to_string()));
            SingleEffectResult::Replace {
                success: false,
                state: None,
                resource_id: ctx.to.id.clone(),
                resolved_attrs: None,
                binding: ctx.effect.binding_name(),
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
#[allow(clippy::type_complexity)]
enum PhaseEffectResult {
    /// Phase 1: Create/Update/Delete completed (wraps BasicEffectResult)
    Basic(BasicEffectResult),
    /// Phase 2: CBD create succeeded
    CbdCreateSuccess {
        idx: usize,
        state: State,
        cascade_states: Vec<(ResourceId, State, HashMap<String, Value>, Option<String>)>,
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
        binding: Option<String>,
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
                    let basic = execute_basic_effect(
                        effect,
                        provider,
                        &binding_snapshot,
                        unresolved,
                        completed_ref,
                        total,
                        observer,
                    )
                    .await;
                    (idx, PhaseEffectResult::Basic(basic))
                });
            }

            if in_flight.is_empty() {
                break;
            }

            let (finished_idx, result) = in_flight.next().await.unwrap();
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::Basic(basic) => {
                    process_basic_result(
                        basic,
                        &mut ExecutionState {
                            success_count: &mut success_count,
                            failure_count: &mut failure_count,
                            applied_states: &mut applied_states,
                            failed_bindings: &mut failed_bindings,
                            successfully_deleted: &mut successfully_deleted,
                            pending_refreshes: &mut pending_refreshes,
                            binding_map: &mut input.binding_map,
                        },
                    );
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
                        let resolved = match resolve_resource_with_source(
                            to,
                            resolve_source,
                            &binding_snapshot,
                        ) {
                            Ok(r) => r,
                            Err(e) => {
                                observer.on_event(&ExecutionEvent::EffectFailed {
                                    effect,
                                    error: &e,
                                    duration: started.elapsed(),
                                    progress,
                                });
                                return (
                                    idx,
                                    started,
                                    PhaseEffectResult::Basic(BasicEffectResult::Failure {
                                        binding: effect.binding_name(),
                                        refresh: None,
                                    }),
                                );
                            }
                        };

                        match provider.create(&resolved).await {
                            Ok(state) => {
                                let mut local_binding_map = binding_snapshot.clone();
                                update_binding_map(
                                    &mut local_binding_map,
                                    &resolved.resolved_attributes(),
                                    to.binding.as_deref(),
                                    &state,
                                );

                                let mut cascade_failed = false;
                                let mut refreshes = Vec::new();
                                let mut cascade_states = Vec::new();
                                for cascade in cascading_updates {
                                    let resolved_to =
                                        match resolve_resource(&cascade.to, &local_binding_map) {
                                            Ok(r) => r,
                                            Err(e) => {
                                                observer.on_event(
                                                    &ExecutionEvent::CascadeUpdateFailed {
                                                        id: &cascade.id,
                                                        error: &e,
                                                    },
                                                );
                                                let cascade_identifier = cascade
                                                    .from
                                                    .identifier
                                                    .as_deref()
                                                    .unwrap_or("");
                                                refreshes.push((
                                                    cascade.id.clone(),
                                                    cascade_identifier.to_string(),
                                                ));
                                                cascade_failed = true;
                                                break;
                                            }
                                        };
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
                                                &resolved_to.resolved_attributes(),
                                                cascade.to.binding.as_deref(),
                                                &cascade_state,
                                            );
                                            cascade_states.push((
                                                cascade.id.clone(),
                                                cascade_state,
                                                resolved_to.resolved_attributes(),
                                                cascade.to.binding.clone(),
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
                        update_binding_map(
                            &mut input.binding_map,
                            &to.resolved_attributes(),
                            to.binding.as_deref(),
                            &state,
                        );
                    }
                    for (cascade_id, cascade_state, cascade_attrs, cascade_binding) in
                        cascade_states
                    {
                        applied_states.insert(cascade_id, cascade_state.clone());
                        update_binding_map(
                            &mut input.binding_map,
                            &cascade_attrs,
                            cascade_binding.as_deref(),
                            &cascade_state,
                        );
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
                PhaseEffectResult::Basic(BasicEffectResult::Failure { binding, .. }) => {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
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
                                rename_to.set_attr(
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
                                let resolved = match resolve_resource_with_source(
                                    to,
                                    resolve_source,
                                    &binding_snapshot,
                                ) {
                                    Ok(r) => r,
                                    Err(e) => {
                                        observer.on_event(&ExecutionEvent::EffectFailed {
                                            effect,
                                            error: &e,
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        return (
                                            idx,
                                            PhaseEffectResult::Basic(BasicEffectResult::Failure {
                                                binding: effect.binding_name(),
                                                refresh: None,
                                            }),
                                        );
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
                                        (
                                            idx,
                                            PhaseEffectResult::NonCbdCreateSuccess {
                                                state,
                                                resource_id: to.id.clone(),
                                                resolved_attrs: resolved.resolved_attributes(),
                                                binding: to.binding.clone(),
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
                    binding,
                } => {
                    success_count += 1;
                    applied_states.insert(resource_id, state.clone());
                    update_binding_map(
                        &mut input.binding_map,
                        &resolved_attrs,
                        binding.as_deref(),
                        &state,
                    );
                }
                PhaseEffectResult::NonCbdCreateFailure { binding } => {
                    failure_count += 1;
                    if let Some(binding) = binding {
                        failed_bindings.insert(binding);
                    }
                }
                PhaseEffectResult::Basic(BasicEffectResult::Failure { binding, .. }) => {
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
        r.binding = Some(binding.to_string());
        for dep in deps {
            r.set_attr(
                format!("ref_{}", dep),
                Value::resource_ref(dep.to_string(), "id".to_string(), vec![]),
            );
        }
        // Save dependency bindings as metadata (normally done by resolver)
        if !deps.is_empty() {
            r.dependency_bindings = deps.iter().map(|d| d.to_string()).collect();
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
        vpc_to.binding = Some("vpc".to_string());

        let subnet_from =
            State::existing(subnet_id.clone(), HashMap::new()).with_identifier("subnet-old");
        let mut subnet_to = Resource::new("test", "subnet");
        subnet_to.binding = Some("subnet".to_string());
        subnet_to.dependency_bindings = vec!["vpc".to_string()];

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
        vpc_to.binding = Some("vpc".to_string());

        let subnet_from =
            State::existing(subnet_id.clone(), HashMap::new()).with_identifier("subnet-old");
        let mut subnet_to = Resource::new("test", "subnet");
        subnet_to.binding = Some("subnet".to_string());
        subnet_to.dependency_bindings = vec!["vpc".to_string()];

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
        tgw_attach.binding = Some("tgw_attach".to_string());
        tgw_attach.dependency_bindings =
            vec!["tgw".to_string(), "vpc".to_string(), "subnet".to_string()];

        // route depends on rt and tgw_attach (but after partial resolution,
        // transit_gateway_id points to ResourceRef { binding: "tgw" })
        let mut route = Resource::new("ec2.route", "my-route");
        route.set_attr(
            "transit_gateway_id".to_string(),
            Value::resource_ref("tgw".to_string(), "id".to_string(), vec![]),
        );
        route.dependency_bindings = vec!["rt".to_string(), "tgw_attach".to_string()];

        // Other resources
        let mut vpc = Resource::new("ec2.vpc", "vpc");
        vpc.binding = Some("vpc".to_string());

        let mut tgw = Resource::new("ec2.transit_gateway", "tgw");
        tgw.binding = Some("tgw".to_string());

        let mut subnet = Resource::new("ec2.subnet", "subnet");
        subnet.binding = Some("subnet".to_string());
        subnet.dependency_bindings = vec!["vpc".to_string()];

        let mut rt = Resource::new("ec2.route_table", "rt");
        rt.binding = Some("rt".to_string());
        rt.dependency_bindings = vec!["vpc".to_string()];

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

    /// Regression test for #1195: Delete effects must respect reverse dependency ordering.
    ///
    /// When deleting resources, children must be deleted before parents.
    /// If subnet depends on vpc, the vpc delete must wait for subnet delete.
    /// Before the fix, `build_dependency_map` returned empty deps for deletes,
    /// allowing parent and child deletes to run concurrently.
    #[test]
    fn test_build_dependency_levels_respects_delete_dependencies() {
        // Scenario: vpc (no deps), subnet (depends on vpc)
        // For creation: subnet depends on vpc → vpc first, then subnet
        // For deletion: vpc delete must wait for subnet delete → subnet first, then vpc
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.vpc", "my-vpc"),
            identifier: "vpc-123".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: Some("vpc".to_string()),
            dependencies: HashSet::new(), // vpc has no deps
        });
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.subnet", "my-subnet"),
            identifier: "subnet-456".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: Some("subnet".to_string()),
            dependencies: HashSet::from(["vpc".to_string()]), // subnet depends on vpc
        });

        let levels = build_dependency_levels(plan.effects(), &HashMap::new());

        // Find levels for each effect
        let vpc_level = levels.iter().position(|group| group.contains(&0)).unwrap();
        let subnet_level = levels.iter().position(|group| group.contains(&1)).unwrap();

        // vpc delete (idx 0) must be at a HIGHER level than subnet delete (idx 1)
        // because vpc must wait for subnet to be deleted first (reverse ordering)
        assert!(
            vpc_level > subnet_level,
            "vpc delete (level {}) must be at a higher level than subnet delete (level {}). \
             Delete ordering must be reversed: children deleted before parents. levels: {:?}",
            vpc_level,
            subnet_level,
            levels
        );
    }

    /// Characterization test for #1306: build_dependency_levels and build_dependency_map
    /// must produce consistent results. This test verifies that after refactoring
    /// build_dependency_levels to reuse build_dependency_map, the level assignments
    /// remain the same.
    #[test]
    fn test_build_dependency_levels_consistent_with_dependency_map() {
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
        let dep_map = build_dependency_map(plan.effects(), &HashMap::new());

        // Verify levels are consistent with the dependency map:
        // For every effect, its level must be greater than all its dependencies' levels.
        for (idx, deps) in &dep_map {
            let idx_level = levels.iter().position(|group| group.contains(idx)).unwrap();
            for dep in deps {
                let dep_level = levels.iter().position(|group| group.contains(dep)).unwrap();
                assert!(
                    idx_level > dep_level,
                    "Effect {} (level {}) must be at a higher level than dependency {} (level {})",
                    idx,
                    idx_level,
                    dep,
                    dep_level
                );
            }
        }

        // Verify the same structure as the existing test
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec![0]);
        assert_eq!(levels[1], vec![1, 2]);
        assert_eq!(levels[2], vec![3]);
    }

    /// Characterization test for #1306: both execution paths (sequential and phased)
    /// must produce the same results for an update effect with binding map propagation.
    #[tokio::test]
    async fn test_update_effect_binding_map_propagation() {
        let provider = MockProvider::new();
        let ra_id = ResourceId::new("test", "a");

        // Create initial state
        let from_state =
            State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-original");
        let to_resource = make_resource("a", &[]);

        let mut plan = Plan::new();
        plan.add(Effect::Update {
            id: ra_id.clone(),
            from: Box::new(from_state),
            to: to_resource,
            changed_attributes: vec!["some_attr".to_string()],
        });

        let updated_state =
            State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-updated");
        provider.push_update(Ok(updated_state));

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
        assert!(result.applied_states.contains_key(&ra_id));

        let events = observer.events();
        assert!(events.iter().any(|e| e.starts_with("started:")));
        assert!(events.iter().any(|e| e.starts_with("succeeded:")));
    }

    /// Regression test for #1548: Delete effect without binding must still be found
    /// by `build_dependency_map` when a Replace(CBD) effect's `from.dependency_bindings`
    /// references it. This happens when an orphan resource loses its `_binding` during
    /// provider refresh.
    #[test]
    fn test_build_dependency_map_delete_without_binding() {
        let mut plan = Plan::new();

        // tgw_a: Delete — binding is None (orphan lost _binding during refresh)
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.transit_gateway", "tgw_a"),
            identifier: "tgw-old".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: None, // <-- the bug: no binding
            dependencies: HashSet::new(),
        });

        // tgw_b: Create
        let mut tgw_b = Resource::new("test", "tgw_b");
        tgw_b.binding = Some("tgw_b".to_string());
        plan.add(Effect::Create(tgw_b));

        // attachment: Replace (CBD) — from depends on tgw_a
        let attachment_from = State::existing(
            ResourceId::new("ec2.transit_gateway_attachment", "attachment"),
            HashMap::new(),
        )
        .with_identifier("attach-old")
        .with_dependency_bindings(vec!["tgw_a".to_string()]);

        let mut attachment_to = Resource::new("ec2.transit_gateway_attachment", "attachment");
        attachment_to.binding = Some("attachment".to_string());
        attachment_to.dependency_bindings = vec!["tgw_b".to_string()];

        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.transit_gateway_attachment", "attachment"),
            from: Box::new(attachment_from),
            to: attachment_to,
            lifecycle: LifecycleConfig {
                create_before_destroy: true,
                ..Default::default()
            },
            changed_create_only: vec!["transit_gateway_id".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let deps = build_dependency_map(plan.effects(), &HashMap::new());

        // tgw_a delete (idx 0) must depend on attachment replace (idx 2)
        // because the old attachment depended on tgw_a
        assert!(
            deps[&0].contains(&2),
            "tgw_a delete should wait for attachment replace even without binding. deps: {:?}",
            deps
        );
    }

    /// Regression test for #1548: Delete→Delete reverse deps must work when the parent
    /// Delete has binding: None (orphan lost _binding during refresh).
    #[test]
    fn test_build_dependency_map_delete_reverse_deps_without_binding() {
        let mut plan = Plan::new();

        // vpc: Delete — binding is None (orphan lost _binding during refresh)
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.vpc", "vpc"),
            identifier: "vpc-123".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: None, // <-- no binding
            dependencies: HashSet::new(),
        });

        // subnet: Delete — depends on "vpc"
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.subnet", "my-subnet"),
            identifier: "subnet-456".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: Some("subnet".to_string()),
            dependencies: HashSet::from(["vpc".to_string()]),
        });

        let deps = build_dependency_map(plan.effects(), &HashMap::new());

        // vpc delete (idx 0) must depend on subnet delete (idx 1)
        assert!(
            deps[&0].contains(&1),
            "vpc delete should depend on subnet delete even without binding. deps: {:?}",
            deps
        );
    }

    /// Regression test for #1195: build_dependency_map also respects delete dependencies.
    #[test]
    fn test_build_dependency_map_respects_delete_dependencies() {
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.vpc", "my-vpc"),
            identifier: "vpc-123".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: Some("vpc".to_string()),
            dependencies: HashSet::new(),
        });
        plan.add(Effect::Delete {
            id: ResourceId::new("ec2.subnet", "my-subnet"),
            identifier: "subnet-456".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: Some("subnet".to_string()),
            dependencies: HashSet::from(["vpc".to_string()]),
        });

        let deps = build_dependency_map(plan.effects(), &HashMap::new());

        // vpc delete (idx 0) must depend on subnet delete (idx 1)
        // because subnet must be deleted before vpc (reverse dependency)
        assert!(
            deps[&0].contains(&1),
            "vpc delete should depend on subnet delete (reverse ordering). deps: {:?}",
            deps
        );
        // subnet delete (idx 1) should NOT depend on vpc delete (idx 0)
        assert!(
            !deps[&1].contains(&0),
            "subnet delete should not depend on vpc delete. deps: {:?}",
            deps
        );
    }

    /// Test that ResourceRef values in dependent resources are resolved using
    /// state attributes from predecessor resources (binding_map propagation).
    #[tokio::test]
    async fn test_resource_ref_resolved_from_predecessor_state() {
        let provider = RecordingMockProvider::new();

        // VPC resource with binding "vpc"
        let mut vpc = Resource::new("test", "my-vpc");
        vpc.binding = Some("vpc".to_string());
        vpc.set_attr("cidr_block", Value::String("10.0.0.0/16".to_string()));
        let vpc_id = vpc.id.clone();

        // Subnet resource that references vpc.vpc_id
        let mut subnet = Resource::new("test", "my-subnet");
        subnet.set_attr(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
        subnet.set_attr("cidr_block", Value::String("10.0.1.0/24".to_string()));
        subnet.dependency_bindings = vec!["vpc".to_string()];
        let subnet_id = subnet.id.clone();

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(subnet));

        // VPC create returns state with vpc_id
        let vpc_state = State::existing(
            vpc_id.clone(),
            vec![("vpc_id".to_string(), Value::String("vpc-12345".to_string()))]
                .into_iter()
                .collect(),
        )
        .with_identifier("vpc-12345");
        provider.push_create(Ok(vpc_state));

        // Subnet create returns state
        let subnet_state = State::existing(
            subnet_id.clone(),
            vec![(
                "subnet_id".to_string(),
                Value::String("subnet-67890".to_string()),
            )]
            .into_iter()
            .collect(),
        )
        .with_identifier("subnet-67890");
        provider.push_create(Ok(subnet_state));

        let input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &HashMap::new(),
            binding_map: HashMap::new(),
            current_states: HashMap::new(),
        };

        let observer = MockObserver::new();
        let result = execute_plan(&provider, input, &observer).await;

        assert_eq!(result.success_count, 2, "Both resources should succeed");
        assert_eq!(result.failure_count, 0, "No failures expected");

        // Check that the subnet received vpc_id = "vpc-12345" (resolved from state)
        let create_calls = provider.create_calls();
        assert_eq!(create_calls.len(), 2, "Should have 2 create calls");

        // First call should be VPC
        assert_eq!(create_calls[0].0, vpc_id.to_string());

        // Second call should be subnet with resolved vpc_id
        assert_eq!(create_calls[1].0, subnet_id.to_string());
        let subnet_attrs = &create_calls[1].1;
        assert_eq!(
            subnet_attrs.get("vpc_id"),
            Some(&Value::String("vpc-12345".to_string())),
            "Subnet's vpc_id should be resolved from VPC state, got: {:?}",
            subnet_attrs.get("vpc_id")
        );
    }

    /// A mock provider that records the resource attributes passed to create().
    type CreateLog = Vec<(String, HashMap<String, Value>)>;

    struct RecordingMockProvider {
        create_results: Mutex<Vec<ProviderResult<State>>>,
        /// Records: (resource_id_string, resolved_attributes)
        create_log: Arc<Mutex<CreateLog>>,
    }

    impl RecordingMockProvider {
        fn new() -> Self {
            Self {
                create_results: Mutex::new(Vec::new()),
                create_log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn push_create(&self, result: ProviderResult<State>) {
            self.create_results.lock().unwrap().push(result);
        }

        fn create_calls(&self) -> Vec<(String, HashMap<String, Value>)> {
            self.create_log.lock().unwrap().clone()
        }
    }

    impl Provider for RecordingMockProvider {
        fn name(&self) -> &'static str {
            "recording_mock"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async {
                Err(ProviderError {
                    message: "not implemented".to_string(),
                    resource_id: None,
                    cause: None,
                    is_timeout: false,
                })
            })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let id_str = resource.id.to_string();
            let attrs = resource.resolved_attributes();
            self.create_log.lock().unwrap().push((id_str, attrs));
            let result = self.create_results.lock().unwrap().remove(0);
            Box::pin(async move { result })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _from: &State,
            _to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async {
                Err(ProviderError {
                    message: "not implemented".to_string(),
                    resource_id: None,
                    cause: None,
                    is_timeout: false,
                })
            })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async {
                Err(ProviderError {
                    message: "not implemented".to_string(),
                    resource_id: None,
                    cause: None,
                    is_timeout: false,
                })
            })
        }
    }

    /// Regression test: when a resource is deleted and a dependent resource is
    /// replaced (CBD), the delete must wait for the replace to complete.
    ///
    /// Scenario (TGW attachment):
    ///   - tgw_a: Delete (binding removed from .crn)
    ///   - tgw_attachment: Replace (CBD) — from depends on tgw_a, to depends on tgw_b
    ///
    /// Without the fix, both execute in parallel and tgw_a delete fails because
    /// the old attachment (which references tgw_a) hasn't been deleted yet.
    #[tokio::test]
    async fn test_delete_waits_for_replace_cbd_of_dependent() {
        let provider = MockProvider::new();
        let tgw_a_id = ResourceId::new("test", "tgw_a");
        let tgw_b_id = ResourceId::new("test", "tgw_b");
        let attachment_id = ResourceId::new("test", "attachment");

        // tgw_a is being deleted (binding removed from desired config)
        let tgw_a_deps: HashSet<String> = HashSet::new();

        // attachment: Replace (CBD)
        // from: depends on tgw_a (recorded in state's dependency_bindings)
        let attachment_from = State::existing(attachment_id.clone(), HashMap::new())
            .with_identifier("attach-old")
            .with_dependency_bindings(vec!["tgw_a".to_string()]);
        // to: depends on tgw_b (different TGW — dependency changed)
        let mut attachment_to = Resource::new("test", "attachment");
        attachment_to.binding = Some("attachment".to_string());
        attachment_to.dependency_bindings = vec!["tgw_b".to_string()];

        let cbd_lifecycle = LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        };

        let mut plan = Plan::new();

        // tgw_b: Create (new resource)
        let mut tgw_b = Resource::new("test", "tgw_b");
        tgw_b.binding = Some("tgw_b".to_string());
        plan.add(Effect::Create(tgw_b));

        // tgw_a: Delete
        plan.add(Effect::Delete {
            id: tgw_a_id.clone(),
            identifier: "tgw-old".to_string(),
            lifecycle: Default::default(),
            binding: Some("tgw_a".to_string()),
            dependencies: tgw_a_deps,
        });

        // attachment: Replace (CBD) — from depends on tgw_a
        plan.add(Effect::Replace {
            id: attachment_id.clone(),
            from: Box::new(attachment_from),
            to: attachment_to,
            lifecycle: cbd_lifecycle,
            changed_create_only: vec!["transit_gateway_id".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        // tgw_b create
        provider.push_create(Ok(ok_state(&tgw_b_id)));
        // attachment CBD: create new
        provider.push_create(Ok(ok_state(&attachment_id)));
        // attachment CBD: delete old
        provider.push_delete(Ok(()));
        // tgw_a: delete (should happen AFTER attachment replace completes)
        provider.push_delete(Ok(()));

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

        let calls = provider.calls();
        // tgw_b create must happen first (attachment depends on it)
        assert_eq!(calls[0], ("create".to_string(), tgw_b_id.to_string()));
        // attachment create (CBD: create before delete)
        assert_eq!(calls[1], ("create".to_string(), attachment_id.to_string()));
        // attachment delete (old)
        assert_eq!(calls[2], ("delete".to_string(), attachment_id.to_string()));
        // tgw_a delete MUST come after attachment replace completes
        assert_eq!(calls[3], ("delete".to_string(), tgw_a_id.to_string()));
    }
}
