//! Phased execution for plans with interdependent Replace effects.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::deps::{find_failed_dependency, get_resource_dependencies};
use crate::effect::Effect;
use crate::provider::Provider;
use crate::resource::{Resource, ResourceId, State, Value};

use super::basic::{
    BasicEffectResult, ExecutionState, count_actionable_effects, execute_basic_effect,
    process_basic_result, queue_state_refresh, refresh_pending_states, resolve_resource,
    resolve_resource_with_source,
};
use super::{ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo};

/// Check if the plan contains multiple Replace effects that depend on each other.
pub(super) fn has_interdependent_replaces(effects: &[Effect]) -> bool {
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
pub(super) fn collect_replace_bindings(effects: &[Effect]) -> HashSet<String> {
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
pub(super) fn topological_sort_replaces(
    effects: &[Effect],
    replace_bindings: &HashSet<String>,
) -> Vec<usize> {
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

/// Build a dependency map for a subset of effects identified by their indices.
///
/// Only considers dependencies between effects in the given subset. Dependencies
/// on effects outside the subset are ignored (assumed already completed).
pub(super) fn build_phase_dependency_map(
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
pub(super) enum PhaseEffectResult {
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
pub(super) async fn execute_effects_phased(
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

                let binding_snapshot = input.bindings.clone();
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
                            bindings: &mut input.bindings,
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

                let binding_snapshot = input.bindings.clone();
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
                                let mut local_bindings = binding_snapshot.clone();
                                local_bindings.record_applied(
                                    to.binding.as_deref(),
                                    &resolved.resolved_attributes(),
                                    &state,
                                );

                                let mut cascade_failed = false;
                                let mut refreshes = Vec::new();
                                let mut cascade_states = Vec::new();
                                for cascade in cascading_updates {
                                    let resolved_to =
                                        match resolve_resource(&cascade.to, &local_bindings) {
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
                                            local_bindings.record_applied(
                                                cascade.to.binding.as_deref(),
                                                &resolved_to.resolved_attributes(),
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
                        input.bindings.record_applied(
                            to.binding.as_deref(),
                            &to.resolved_attributes(),
                            &state,
                        );
                    }
                    for (cascade_id, cascade_state, cascade_attrs, cascade_binding) in
                        cascade_states
                    {
                        applied_states.insert(cascade_id, cascade_state.clone());
                        input.bindings.record_applied(
                            cascade_binding.as_deref(),
                            &cascade_attrs,
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

                let binding_snapshot = input.bindings.clone();
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
                    input
                        .bindings
                        .record_applied(binding.as_deref(), &resolved_attrs, &state);
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
