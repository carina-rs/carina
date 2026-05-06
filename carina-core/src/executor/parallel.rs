//! Dependency computation and parallel effect execution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::deps::find_failed_dependency;
use crate::effect::Effect;
use crate::provider::Provider;
use crate::resource::{Resource, ResourceId, State};

use super::basic::{
    ExecutionState, count_actionable_effects, execute_basic_effect, process_basic_result,
    refresh_pending_states,
};
use super::phased::DepResolver;
use super::replace::{ReplaceContext, SingleEffectResult, execute_replace_parallel};
use super::{ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo};

/// Build a dependency map: for each effect index, which other effect indices it depends on.
pub(super) fn build_dependency_map(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> HashMap<usize, HashSet<usize>> {
    // Build binding -> effect index mapping
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    // Fallback: ResourceId name -> effect index for Delete effects without bindings.
    // When a resource loses its `let` binding (e.g., becomes anonymous in a new .crn),
    // the Delete effect has binding: None. But other effects may still reference the
    // old binding name via state-recorded dependency_bindings. The name-based lookup
    // allows resolving these dependencies.
    let mut name_to_delete_idx: HashMap<String, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        if let Some(binding) = effect.binding_name() {
            binding_to_idx.insert(binding, idx);
        }
        if let Effect::Delete { id, binding, .. } = effect
            && binding.is_none()
        {
            name_to_delete_idx.insert(id.name_str().to_string(), idx);
        }
    }

    let resolver = DepResolver::new(&binding_to_idx, unresolved_resources, None);

    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        let mut dep_indices = HashSet::new();
        if let Some(resource) = effect.resource() {
            resolver.collect_from_resource(resource, &mut dep_indices);
            if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                resolver.collect_from_resource(unresolved, &mut dep_indices);
            }
        }
        deps_of.insert(idx, dep_indices);
    }

    // Helper: look up effect index by binding name, falling back to Delete-by-name.
    let lookup_idx = |binding: &str| -> Option<usize> {
        binding_to_idx
            .get(binding)
            .or_else(|| name_to_delete_idx.get(binding))
            .copied()
    };

    // For Delete effects, add reverse dependencies: if subnet depends on vpc,
    // the vpc delete must wait for subnet delete (children deleted before parents).
    let mut reverse_deps: Vec<(usize, usize)> = Vec::new();
    for (idx, effect) in effects.iter().enumerate() {
        if let Effect::Delete { dependencies, .. } = effect {
            for dep_binding in dependencies {
                if let Some(dep_idx) = lookup_idx(dep_binding) {
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
                if let Some(dep_idx) = lookup_idx(dep_binding)
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

#[cfg(test)]
pub(super) fn build_dependency_levels(
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

pub(super) async fn execute_effects_sequential(
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

            // Snapshot bindings for this effect's resolution
            let binding_snapshot = input.bindings.clone();
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
                                bindings: &binding_snapshot,
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
                        bindings: &mut input.bindings,
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
                        input
                            .bindings
                            .record_applied(binding.as_deref(), attrs, state);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ResourceKind, Value};

    /// Mirror of #2543's phased-executor test for the unphased dependency map:
    /// virtual module-attribute proxies must be transparently followed to the
    /// underlying resources their attributes reference.
    #[test]
    fn build_dependency_map_follows_virtual_module_binding() {
        let mut role = Resource::with_provider("awscc", "iam.Role", "bootstrap.role");
        role.binding = Some("bootstrap.role".to_string());

        let mut virt = Resource::with_provider("_virtual", "_virtual", "bootstrap");
        virt.binding = Some("bootstrap".to_string());
        virt.kind = ResourceKind::Virtual {
            module_name: "github-oidc".to_string(),
            instance: "bootstrap".to_string(),
        };
        virt.set_attr(
            "role_name",
            Value::resource_ref("bootstrap.role", "role_name", vec![]),
        );

        let mut role_policy = Resource::with_provider("awscc", "iam.RolePolicy", "rp");
        role_policy.set_attr(
            "role_name",
            Value::resource_ref("bootstrap", "role_name", vec![]),
        );

        let effects = vec![
            Effect::Create(role.clone()),
            Effect::Create(role_policy.clone()),
        ];

        let mut unresolved: HashMap<ResourceId, Resource> = HashMap::new();
        unresolved.insert(role.id.clone(), role.clone());
        unresolved.insert(virt.id.clone(), virt);
        unresolved.insert(role_policy.id.clone(), role_policy);

        let deps_of = build_dependency_map(&effects, &unresolved);

        assert!(
            deps_of[&1].contains(&0),
            "RolePolicy (idx 1) must depend on Role (idx 0) via the bootstrap virtual binding; got: {:?}",
            deps_of[&1],
        );
    }
}
