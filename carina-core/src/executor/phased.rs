//! Phased execution for plans with interdependent Replace effects.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::deps::{find_failed_dependency, get_resource_dependencies};
use crate::effect::Effect;
use crate::provider::{CreateRequest, DeleteRequest, Provider, UpdateRequest};
use crate::resource::{ConcreteValue, Resource, ResourceId, State, Value};

use super::basic::{
    BasicEffectCtx, BasicEffectResult, ExecutionState, RenormalizePipeline,
    count_actionable_effects, execute_basic_effect, process_basic_result, queue_state_refresh,
    refresh_pending_states, resolve_resource, resolve_resource_with_source,
};
use super::replace::{compute_full_diff_patch, single_attribute_patch};
use super::{ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo};

/// Check if the plan contains multiple Replace effects that depend on each other.
pub(super) fn has_interdependent_replaces(effects: &[Effect]) -> bool {
    let replace_bindings = collect_replace_bindings(effects);
    if replace_bindings.is_empty() {
        return false;
    }

    for effect in effects {
        if let Effect::Replace { to, .. } = effect {
            for dep in get_resource_dependencies(to) {
                if replace_bindings.contains(&dep) {
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

    // Build adjacency: for each replace effect, find which other replace effects it depends on.
    // Use the unioned `get_resource_dependencies` so explicit
    // `directives.depends_on` edges participate alongside value refs (#2875).
    let mut deps: HashMap<usize, Vec<usize>> = HashMap::new();
    for &idx in &replace_indices {
        let effect = &effects[idx];
        if let Effect::Replace { to, .. } = effect {
            let dep_indices: Vec<usize> = get_resource_dependencies(to)
                .iter()
                .filter(|b| replace_bindings.contains(b.as_str()))
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
///
/// `Virtual` resources (the synthetic bindings module calls expose for their
/// `attributes { }` block) have no Effect and would be invisible to a direct
/// `binding -> effect index` lookup. To preserve the dependency edge from a
/// caller through a module's attribute to the underlying resource, virtual
/// bindings are expanded transitively into the resource bindings their own
/// attributes reference (#2543).
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

    let resolver = DepResolver::new(&binding_to_idx, unresolved_resources, Some(&phase_set));

    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for &idx in phase_indices {
        let mut dep_indices = HashSet::new();
        let effect = &effects[idx];
        if let Some(resource) = effect.resource() {
            resolver.collect_from_resource(resource, &mut dep_indices);
            if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                resolver.collect_from_resource(unresolved, &mut dep_indices);
            }
        }
        deps_of.insert(idx, dep_indices);
    }
    deps_of
}

/// Resolve binding-name dependencies to the effect indices they reach,
/// expanding any [`VirtualResource`] proxy bindings transparently
/// through their own attribute references (#2543).
///
/// `virtuals_by_binding` now holds only virtuals (typestate-split
/// #3178): the previous mixed `HashMap<&str, &Resource>` plus a
/// runtime `matches!(res.kind, Virtual { .. })` check is replaced
/// by a slice of one type. A binding being present in
/// `virtuals_by_binding` *is* the "this is a virtual" condition; no
/// runtime kind probe is needed.
pub(super) struct DepResolver<'a> {
    binding_to_idx: &'a HashMap<String, usize>,
    /// Virtual resources owned by the resolver, keyed by their
    /// `binding` name. Owned rather than `&'a VirtualResource`
    /// because `VirtualResource::try_from(&Resource)` returns an
    /// owned wrapper — keeping a borrow would force a
    /// self-referential struct. The clone cost is bounded by the
    /// number of virtuals in the config (typically small).
    virtuals_by_binding: HashMap<String, crate::resource::VirtualResource>,
    /// `Some` filters output indices to those in the phase; `None` retains
    /// every reachable index.
    phase_set: Option<&'a HashSet<usize>>,
}

impl<'a> DepResolver<'a> {
    pub(super) fn new(
        binding_to_idx: &'a HashMap<String, usize>,
        unresolved_resources: &'a HashMap<ResourceId, Resource>,
        phase_set: Option<&'a HashSet<usize>>,
    ) -> Self {
        // Project the mixed `unresolved_resources` map to a
        // virtual-only `virtuals_by_binding` view via
        // `VirtualResource::try_from`. Managed / DataSource arms
        // fail the conversion and are silently filtered — that is
        // the typestate-split equivalent of the legacy
        // `matches!(res.kind, Virtual { .. })` check inside
        // `expand`.
        //
        // The legacy code recorded all kinds in
        // `binding_to_resource` and gated virtual-expansion on a
        // runtime kind check at lookup time. Restricting the map
        // to virtuals at construction time keeps the observable
        // behaviour identical (only virtuals were ever traversed
        // by the legacy gate) and removes the runtime probe.
        let virtuals_by_binding: HashMap<String, crate::resource::VirtualResource> =
            unresolved_resources
                .values()
                .filter_map(|r| {
                    let binding = r.binding.clone()?;
                    crate::resource::VirtualResource::try_from(r)
                        .ok()
                        .map(|v| (binding, v))
                })
                .collect();
        Self {
            binding_to_idx,
            virtuals_by_binding,
            phase_set,
        }
    }

    /// Walk a resource's dependencies (via `get_resource_dependencies`) and
    /// merge the reached effect indices into `out`.
    pub(super) fn collect_from_resource(&self, resource: &Resource, out: &mut HashSet<usize>) {
        let dep_bindings = get_resource_dependencies(resource);
        let mut visited: HashSet<&str> = HashSet::new();
        for binding in &dep_bindings {
            self.expand(binding.as_str(), out, &mut visited);
        }
    }

    /// Recursive dependency walk. The `'b` lifetime is bound to the
    /// `&self` borrow at the call site so the borrowed keys live
    /// inside the resolver (`virtuals_by_binding` / `binding_to_idx`).
    fn expand<'b>(
        &'b self,
        binding: &'b str,
        out: &mut HashSet<usize>,
        visited: &mut HashSet<&'b str>,
    ) {
        if !visited.insert(binding) {
            return;
        }
        if let Some(&idx) = self.binding_to_idx.get(binding) {
            if self.phase_set.is_none_or(|s| s.contains(&idx)) {
                out.insert(idx);
            }
            return;
        }
        // No effect for this binding. If it names a `VirtualResource`
        // (a module's attributes-block proxy), follow the
        // references in its own attributes to the underlying
        // resources the module exposes. The typed map answers
        // "is this a virtual?" by presence — no `matches!` probe.
        let Some(virt) = self.virtuals_by_binding.get(binding) else {
            return;
        };
        // `get_virtual_resource_dependencies` returns owned `String`s,
        // but the visit set borrows from this resolver's keys to
        // avoid per-binding allocation. Re-borrow each inner
        // binding from the resolver's own keys so the borrow
        // lifetime matches `'b` (the `&self` borrow lifetime).
        for inner in crate::deps::get_virtual_resource_dependencies(virt) {
            let key: &'b str =
                if let Some((k, _)) = self.virtuals_by_binding.get_key_value(inner.as_str()) {
                    k.as_str()
                } else if let Some((k, _)) = self.binding_to_idx.get_key_value(inner.as_str()) {
                    k.as_str()
                } else {
                    // Unknown binding: not in the resource graph, skip.
                    continue;
                };
            self.expand(key, out, visited);
        }
    }
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

                // Phase 1 only dispatches `Create`/`Update`/`Delete` to
                // `execute_basic_effect`. State-only effects (`Move`/
                // `Import`/`Remove`) and `Wait` are routed elsewhere
                // (the CLI's `execute_state_only_effects` step, or the
                // sequential path's Wait branch). The previous
                // `&Effect` signature let them slip through and trip an
                // `unreachable!()` (carina#3164); narrowing via
                // `as_basic()` makes the contract type-level. The
                // outer `phase1_indices` filter still excludes
                // `Replace` and `Read` so they reach their dedicated
                // phases; everything else that isn't basic ends up
                // here and is silently skipped from the basic
                // executor's point of view.
                let Some(basic_effect) = effect.as_basic() else {
                    completed_indices.insert(idx);
                    continue;
                };

                let binding_snapshot = input.bindings.clone();
                let unresolved = &input.unresolved_resources;
                let pipeline = RenormalizePipeline {
                    normalizer: input.normalizer,
                    factories: input.factories,
                    schemas: input.schemas,
                };
                let completed_ref = &completed;

                in_flight.push(async move {
                    let basic = execute_basic_effect(
                        basic_effect,
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
                matches!(&effects[idx], Effect::Replace { directives, .. } if directives.create_before_destroy)
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
                let pipeline = RenormalizePipeline {
                    normalizer: input.normalizer,
                    factories: input.factories,
                    schemas: input.schemas,
                };

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
                            &pipeline,
                        )
                        .await
                        {
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

                        match provider
                            .create(
                                &to.id,
                                CreateRequest {
                                    resource: resolved.clone(),
                                },
                            )
                            .await
                        {
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
                                    let resolved_to = match resolve_resource(
                                        &cascade.to,
                                        &local_bindings,
                                        &pipeline,
                                    )
                                    .await
                                    {
                                        Ok(r) => r,
                                        Err(e) => {
                                            observer.on_event(
                                                &ExecutionEvent::CascadeUpdateFailed {
                                                    id: &cascade.id,
                                                    error: &e,
                                                },
                                            );
                                            let cascade_identifier =
                                                cascade.from.identifier.as_deref().unwrap_or("");
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
                                    let cascade_patch =
                                        compute_full_diff_patch(&cascade.from, &resolved_to);
                                    let cascade_request = UpdateRequest {
                                        from: (*cascade.from).clone(),
                                        patch: cascade_patch,
                                    };
                                    match provider
                                        .update(&cascade.id, cascade_identifier, cascade_request)
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
                if let Effect::Replace { directives, .. } = effect {
                    // Skip if dependency failed
                    if find_failed_dependency(effect, &failed_bindings).is_some() {
                        return false;
                    }
                    // For CBD, skip if create didn't succeed
                    if directives.create_before_destroy && !cbd_create_states.contains_key(&idx) {
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
        let resolver = DepResolver::new(
            &binding_to_idx,
            input.unresolved_resources,
            Some(&phase_set),
        );
        let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
        for &idx in &delete_indices {
            let effect = &effects[idx];
            let mut dep_indices = HashSet::new();
            if let Some(resource) = effect.resource() {
                resolver.collect_from_resource(resource, &mut dep_indices);
                if let Some(unresolved) = input.unresolved_resources.get(effect.resource_id()) {
                    resolver.collect_from_resource(unresolved, &mut dep_indices);
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
                let is_cbd = matches!(effect, Effect::Replace { directives, .. } if directives.create_before_destroy);

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
                        directives,
                        ..
                    } = effect
                    {
                        let identifier = from.identifier.as_deref().unwrap_or("");
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
                let pipeline = RenormalizePipeline {
                    normalizer: input.normalizer,
                    factories: input.factories,
                    schemas: input.schemas,
                };

                if let Effect::Replace {
                    id,
                    to,
                    directives,
                    temporary_name,
                    ..
                } = effect
                {
                    let effect_started = replace_start_times
                        .get(&idx)
                        .copied()
                        .unwrap_or_else(Instant::now);

                    if directives.create_before_destroy {
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
                                let rename_patch = single_attribute_patch(
                                    temp.attribute.clone(),
                                    Value::Concrete(ConcreteValue::String(
                                        temp.original_value.clone(),
                                    )),
                                );
                                let rename_request = UpdateRequest {
                                    from: state.clone(),
                                    patch: rename_patch,
                                };
                                match provider.update(&id, new_identifier, rename_request).await {
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
                                    &pipeline,
                                )
                                .await
                                {
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

                                match provider
                                    .create(
                                        &to.id,
                                        CreateRequest {
                                            resource: resolved.clone(),
                                        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ResourceId, ResourceKind, Value};

    /// Reproduces #2543: when a resource depends on `<module-instance>.<attr>`
    /// (where the module-instance binding is a `Virtual` resource exposing the
    /// module's `attributes { }`), the executor's phase dependency map drops
    /// the dep silently — virtual resources have no Effect entry to look up.
    /// The fix must follow the virtual binding through to the underlying
    /// resource(s) it references.
    #[test]
    fn build_phase_dependency_map_follows_virtual_module_binding() {
        let mut role = Resource::with_provider("awscc", "iam.Role", "bootstrap.role", None);
        role.binding = Some("bootstrap.role".to_string());

        let mut virt = Resource::with_provider("_virtual", "_virtual", "bootstrap", None);
        virt.binding = Some("bootstrap".to_string());
        virt.kind = ResourceKind::Virtual {
            module_name: "github-oidc".to_string(),
            instance: "bootstrap".to_string(),
        };
        // The virtual exposes `role_name = role.role_name`, which after intra-module
        // rewriting becomes `bootstrap.role.role_name`.
        virt.set_attr(
            "role_name",
            Value::resource_ref("bootstrap.role", "role_name", vec![]),
        );

        let mut role_policy = Resource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        role_policy.set_attr(
            "role_name",
            Value::resource_ref("bootstrap", "role_name", vec![]),
        );

        let effects = vec![
            Effect::Create(role.clone()),
            Effect::Create(role_policy.clone()),
        ];
        let phase_indices: Vec<usize> = vec![0, 1];

        let mut unresolved: HashMap<ResourceId, Resource> = HashMap::new();
        unresolved.insert(role.id.clone(), role.clone());
        unresolved.insert(virt.id.clone(), virt.clone());
        unresolved.insert(role_policy.id.clone(), role_policy.clone());

        let deps_of = build_phase_dependency_map(&effects, &phase_indices, &unresolved);

        assert!(
            deps_of[&1].contains(&0),
            "RolePolicy (idx 1) must depend on Role (idx 0) via the bootstrap virtual binding; got: {:?}",
            deps_of[&1],
        );
    }

    /// Module nesting: the outer caller references a virtual binding whose own
    /// attribute references another virtual binding. The dep walk must drill
    /// through both layers to the underlying resource.
    #[test]
    fn build_phase_dependency_map_follows_nested_virtual_module_bindings() {
        let mut role = Resource::with_provider("awscc", "iam.Role", "outer.inner.role", None);
        role.binding = Some("outer.inner.role".to_string());

        let mut inner_virt = Resource::with_provider("_virtual", "_virtual", "outer.inner", None);
        inner_virt.binding = Some("outer.inner".to_string());
        inner_virt.kind = ResourceKind::Virtual {
            module_name: "inner-mod".to_string(),
            instance: "outer.inner".to_string(),
        };
        inner_virt.set_attr(
            "role_name",
            Value::resource_ref("outer.inner.role", "role_name", vec![]),
        );

        let mut outer_virt = Resource::with_provider("_virtual", "_virtual", "outer", None);
        outer_virt.binding = Some("outer".to_string());
        outer_virt.kind = ResourceKind::Virtual {
            module_name: "outer-mod".to_string(),
            instance: "outer".to_string(),
        };
        outer_virt.set_attr(
            "role_name",
            Value::resource_ref("outer.inner", "role_name", vec![]),
        );

        let mut caller = Resource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        caller.set_attr(
            "role_name",
            Value::resource_ref("outer", "role_name", vec![]),
        );

        let effects = vec![Effect::Create(role.clone()), Effect::Create(caller.clone())];
        let phase_indices: Vec<usize> = vec![0, 1];

        let mut unresolved: HashMap<ResourceId, Resource> = HashMap::new();
        unresolved.insert(role.id.clone(), role);
        unresolved.insert(inner_virt.id.clone(), inner_virt);
        unresolved.insert(outer_virt.id.clone(), outer_virt);
        unresolved.insert(caller.id.clone(), caller);

        let deps_of = build_phase_dependency_map(&effects, &phase_indices, &unresolved);

        assert!(
            deps_of[&1].contains(&0),
            "caller must depend on Role through two virtual layers (outer → outer.inner → outer.inner.role); got: {:?}",
            deps_of[&1],
        );
    }

    /// #2875: Replace topological sort must respect explicit
    /// `directives.depends_on` edges, not only `dependency_bindings`
    /// (which is value-ref-only post-#2823).
    #[test]
    fn topological_sort_replaces_respects_depends_on() {
        use crate::resource::{Directives, State};

        let mut role = Resource::with_provider("test", "iam.Role", "role", None);
        role.binding = Some("role".to_string());
        let mut bucket = Resource::with_provider("test", "s3.Bucket", "bucket", None);
        bucket.binding = Some("bucket".to_string());
        bucket.directives = Directives {
            depends_on: vec!["role".to_string()],
            ..Directives::default()
        };

        let role_state = State::not_found(role.id.clone());
        let bucket_state = State::not_found(bucket.id.clone());

        let role_replace = Effect::Replace {
            id: role.id.clone(),
            from: Box::new(role_state),
            to: role.clone(),
            directives: Directives::default(),
            changed_create_only: vec!["role_name".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };
        let bucket_replace = Effect::Replace {
            id: bucket.id.clone(),
            from: Box::new(bucket_state),
            to: bucket.clone(),
            directives: Directives::default(),
            changed_create_only: vec!["bucket_name".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };

        let effects = vec![bucket_replace, role_replace];
        let replace_bindings = collect_replace_bindings(&effects);
        assert!(
            has_interdependent_replaces(&effects),
            "depends_on-only edge between two Replaces should count as interdependent"
        );
        let sorted = topological_sort_replaces(&effects, &replace_bindings);
        let role_pos = sorted.iter().position(|&i| i == 1).unwrap();
        let bucket_pos = sorted.iter().position(|&i| i == 0).unwrap();
        assert!(
            role_pos < bucket_pos,
            "role Replace must come before bucket Replace; sorted={:?}",
            sorted
        );
    }
}
