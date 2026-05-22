//! Dependency computation and parallel effect execution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::deps::find_failed_dependency;
use crate::effect::{Effect, WaitTarget};
use crate::provider::Provider;
use crate::resource::{ManagedResource, ResourceId, State, Value};

use super::basic::{
    BasicEffectCtx, ExecutionState, RenormalizePipeline, count_actionable_effects,
    execute_basic_effect, process_basic_result, refresh_pending_states,
};
use super::phased::DepResolver;
use super::replace::{ReplaceContext, SingleEffectResult, execute_replace_parallel};
use super::{ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo};

/// Build a dependency map: for each effect index, which other effect indices it depends on.
pub(super) fn build_dependency_map(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, ManagedResource>,
    virtual_resources: &[crate::resource::VirtualResource],
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

    let resolver = DepResolver::new(&binding_to_idx, virtual_resources, None);

    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        let mut dep_indices = HashSet::new();
        if effect.resource_like().is_some() {
            resolver.collect_from_effect(effect, &mut dep_indices);
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

    // Wait dependencies: each `Effect::Wait` depends on the effect that
    // produces its target (Create / Update / Replace on `target_id`) so
    // the wait does not start polling before the target has been
    // created. Explicit `depends_on = [...]` entries from the wait
    // block layer on top.
    for (idx, effect) in effects.iter().enumerate() {
        if let Effect::Wait { target_id, .. } = effect {
            // Edge from the wait to its target's binding-effect (if
            // present in the plan). The target may already exist (no
            // Create effect in this plan) — then the wait is free to
            // start immediately, which is what we want for refresh-only
            // applies.
            if let Some(target_binding) = lookup_idx(target_id.name_str()) {
                deps_of.entry(idx).or_default().insert(target_binding);
            }
            // Honour `depends_on = [...]` declared in the wait block.
            for dep_binding in effect.explicit_dependencies() {
                if let Some(dep_idx) = lookup_idx(&dep_binding) {
                    deps_of.entry(idx).or_default().insert(dep_idx);
                }
            }
            // Defensive: ensure the wait has an entry even when it has
            // no resolved deps (an isolated wait still needs to appear
            // in deps_of so the scheduler's `&deps_of[&idx]` lookup
            // doesn't panic).
            deps_of.entry(idx).or_default();
        }
    }

    deps_of
}

#[cfg(test)]
pub(super) fn build_dependency_levels(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, ManagedResource>,
    virtual_resources: &[crate::resource::VirtualResource],
) -> Vec<Vec<usize>> {
    let deps_of = build_dependency_map(effects, unresolved_resources, virtual_resources);

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

    let deps_of =
        build_dependency_map(effects, input.unresolved_resources, input.virtual_resources);

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
            // Resolve the wait target's identifier *now*, before the
            // dispatch closure: a target created in this same run has no
            // plan-time identifier (`WaitTarget::ResolvedAtApply`), so
            // we read it from the just-completed effect's state held in
            // `applied_states`. The scheduler guarantees the producing
            // Create/Update/Replace ran before this Wait is dispatched
            // (see the Wait-dependency edges above), so the entry is
            // present. Falls back to no identifier only when the target
            // was never produced in this plan (refresh-only apply).
            // carina#3119.
            let wait_identifier: Option<String> = match effect {
                Effect::Wait {
                    target_id, target, ..
                } => match target {
                    WaitTarget::Known(id) => Some(id.clone()),
                    WaitTarget::ResolvedAtApply => applied_states
                        .get(target_id)
                        .and_then(|s| s.identifier.clone()),
                },
                _ => None,
            };
            let unresolved = &input.unresolved_resources;
            let pipeline = RenormalizePipeline {
                normalizer: input.normalizer,
                factories: input.factories,
                schemas: input.schemas,
            };
            let completed_ref = &completed;

            in_flight.push(async move {
                let result = match effect.as_basic() {
                    // `BasicEffect` is the type-level contract for
                    // `execute_basic_effect`: any Create/Update/Delete
                    // narrows here, and any non-basic variant falls
                    // through to the `None` arm so it can't accidentally
                    // be dispatched (carina#3164). The compiler enforces
                    // exhaustiveness on the outer `match effect { ... }`
                    // below.
                    Some(basic) => SingleEffectResult::Basic(
                        execute_basic_effect(
                            basic,
                            &BasicEffectCtx {
                                provider,
                                bindings: &binding_snapshot,
                                unresolved,
                                pipeline: &pipeline,
                                completed: completed_ref,
                                total,
                            },
                            observer,
                        )
                        .await,
                    ),
                    None => match effect {
                        Effect::Create(_) | Effect::Update { .. } | Effect::Delete { .. } => {
                            // `as_basic()` returns `Some` for exactly these
                            // three variants; they're handled by the `Some`
                            // arm above.
                            unreachable!("Create/Update/Delete are narrowed by as_basic()")
                        }
                        Effect::Replace {
                            id,
                            from,
                            to,
                            directives,
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
                                    directives,
                                    cascading_updates,
                                    temporary_name: temporary_name.as_ref(),
                                    bindings: &binding_snapshot,
                                    unresolved,
                                    pipeline: &pipeline,
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
                        Effect::Wait {
                            binding,
                            target_id,
                            until,
                            timeout,
                            interval,
                            ..
                        } => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted { effect });
                            let progress = ProgressInfo {
                                completed: c,
                                total,
                            };
                            match super::wait::execute_wait_effect(
                                provider,
                                target_id,
                                wait_identifier.as_deref(),
                                until,
                                *timeout,
                                *interval,
                            )
                            .await
                            {
                                Ok(state) => {
                                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                                        effect,
                                        state: Some(&state),
                                        duration: started.elapsed(),
                                        progress,
                                    });
                                    SingleEffectResult::Wait {
                                        success: true,
                                        binding: binding.clone(),
                                        target_state: Some(state),
                                    }
                                }
                                Err(e) => {
                                    let err_msg = e.to_string();
                                    observer.on_event(&ExecutionEvent::EffectFailed {
                                        effect,
                                        error: &err_msg,
                                        duration: started.elapsed(),
                                        progress,
                                    });
                                    SingleEffectResult::Wait {
                                        success: false,
                                        binding: binding.clone(),
                                        target_state: None,
                                    }
                                }
                            }
                        }
                    },
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
            SingleEffectResult::Wait {
                success,
                binding,
                target_state,
            } => {
                if success {
                    success_count += 1;
                    if let Some(state) = target_state {
                        // Register the captured target snapshot under the
                        // wait's *synthetic* ResourceId so the downstream
                        // resolution layer can deref `<binding>.<attr>` —
                        // and under the binding name in `bindings` so
                        // resolve_refs sees the same attribute map. Wait
                        // effects do not persist to the state file
                        // (handled by `state_writeback_should_skip`).
                        let synthetic = ResourceId::new("__wait", &binding);
                        let attrs: HashMap<String, Value> = state
                            .attributes
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        input
                            .bindings
                            .record_applied(Some(&binding), &attrs, &state);
                        applied_states.insert(synthetic, state);
                    }
                } else {
                    failure_count += 1;
                    failed_bindings.insert(binding);
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
    use crate::resource::{Value, VirtualResource};

    /// Mirror of #2543's phased-executor test for the unphased dependency map:
    /// virtual module-attribute proxies must be transparently followed to the
    /// underlying resources their attributes reference.
    #[test]
    fn build_dependency_map_follows_virtual_module_binding() {
        let mut role = ManagedResource::with_provider("awscc", "iam.Role", "bootstrap.role", None);
        role.binding = Some("bootstrap.role".to_string());

        // carina#3181: virtual resources are a distinct typestate.
        let mut virt_attrs = indexmap::IndexMap::new();
        virt_attrs.insert(
            "role_name".to_string(),
            Value::resource_ref("bootstrap.role", "role_name", vec![]),
        );
        let virt = VirtualResource {
            id: ResourceId::with_provider("_virtual", "_virtual", "bootstrap", None),
            attributes: virt_attrs,
            binding: Some("bootstrap".to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "github-oidc".to_string(),
            instance: "bootstrap".to_string(),
            quoted_string_attrs: std::collections::HashSet::new(),
        };

        let mut role_policy = ManagedResource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        role_policy.set_attr(
            "role_name",
            Value::resource_ref("bootstrap", "role_name", vec![]),
        );

        let effects = vec![
            Effect::Create(role.clone()),
            Effect::Create(role_policy.clone()),
        ];

        let mut unresolved: HashMap<ResourceId, ManagedResource> = HashMap::new();
        unresolved.insert(role.id.clone(), role.clone());
        unresolved.insert(role_policy.id.clone(), role_policy);

        let deps_of = build_dependency_map(&effects, &unresolved, &[virt]);

        assert!(
            deps_of[&1].contains(&0),
            "RolePolicy (idx 1) must depend on Role (idx 0) via the bootstrap virtual binding; got: {:?}",
            deps_of[&1],
        );
    }
}
