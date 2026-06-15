//! Phased execution for plans with interdependent Replace effects.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::deps::{find_failed_dependency, get_resource_dependencies};
use crate::effect::Effect;
use crate::provider::{CreateRequest, DeleteRequest, Provider, UpdateRequest};
use crate::resource::{ConcreteValue, Resource, ResourceId, State, Value};

use super::basic::{
    BasicEffectCtx, BasicEffectResult, ExecutionState, RenormalizePipeline,
    count_actionable_effects, execute_basic_effect, process_basic_result, queue_state_refresh,
    refresh_pending_states, resolve_resource, resolve_resource_with_source,
};
use super::expand::expand_deferred_for_effects;
use super::parallel::is_runtime_dispatchable;
use super::replace::{compute_full_diff_patch, single_attribute_patch};
use super::wait::{
    AppliedStates, SKIP_REASON_CANCELLED, UnsatisfiableReason, WaitAwareInFlight, WaitOutcome,
    count_effectively_undispatched, resolve_wait_identifier, unsatisfiable_reason_message,
    wait_failure_message,
};
use super::{
    ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo,
    UnresolvedResource,
};

fn emit_cancelled_skips_with_progress<F>(
    effects: &[Effect],
    indices: &[usize],
    dispatched: &mut HashSet<usize>,
    completed_indices: &mut HashSet<usize>,
    skip_count: &mut usize,
    observer: &dyn ExecutionObserver,
    mut progress_for: F,
) where
    F: FnMut(usize) -> ProgressInfo,
{
    for &idx in indices {
        if dispatched.contains(&idx) {
            continue;
        }
        dispatched.insert(idx);
        completed_indices.insert(idx);
        observer.on_event(&ExecutionEvent::EffectSkipped {
            effect: &effects[idx],
            reason: SKIP_REASON_CANCELLED,
            progress: progress_for(idx),
        });
        *skip_count += 1;
    }
}

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
/// on effects outside the subset are ignored (assumed already completed). Wait
/// blockers, including both target and explicit wait-block dependencies, are
/// handled uniformly by `DepResolver::collect_from_effect` via
/// `Effect::blocking_bindings()`.
///
/// `Virtual` resources (the synthetic bindings module calls expose for their
/// `attributes { }` block) have no Effect and would be invisible to a direct
/// `binding -> effect index` lookup. To preserve the dependency edge from a
/// caller through a module's attribute to the underlying resource, composition
/// bindings are expanded transitively into the resource bindings their own
/// attributes reference (#2543).
pub(super) fn build_phase_dependency_map(
    effects: &[Effect],
    phase_indices: &[usize],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
) -> HashMap<usize, HashSet<usize>> {
    // Build binding -> effect index mapping for effects in this phase
    let phase_set: HashSet<usize> = phase_indices.iter().copied().collect();
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    for &idx in phase_indices {
        if let Some(binding) = effects[idx].binding_name() {
            binding_to_idx.insert(binding, idx);
        }
    }

    let resolver = DepResolver::new(&binding_to_idx, compositions, Some(&phase_set));

    let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for &idx in phase_indices {
        let mut dep_indices = HashSet::new();
        let effect = &effects[idx];
        match effect {
            Effect::Wait { .. } | Effect::ExpandDeferredFor { .. } => {
                resolver.collect_from_effect(effect, &mut dep_indices);
            }
            _ if effect.as_resource_ref().is_some() => {
                resolver.collect_from_effect(effect, &mut dep_indices);
                if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                    resolver.collect_from_resource(unresolved.as_resource(), &mut dep_indices);
                }
            }
            _ => {}
        }
        deps_of.insert(idx, dep_indices);
    }
    deps_of
}

/// Resolve binding-name dependencies to the effect indices they reach,
/// expanding any [`Composition`] proxy bindings transparently
/// through their own attribute references (#2543).
///
/// carina#3181: composition resources are a distinct typestate, supplied to
/// [`DepResolver::new`] as their own slice and indexed by `binding`
/// name. A binding present in `compositions_by_binding` *is* the "this is a
/// composition" condition — the `expand` walk follows such a binding through
/// the composition's own attribute references.
pub(super) struct DepResolver<'a> {
    binding_to_idx: &'a HashMap<String, usize>,
    /// Virtual resources owned by the resolver, keyed by their
    /// `binding` name.
    compositions_by_binding: HashMap<String, crate::resource::Composition>,
    /// `Some` filters output indices to those in the phase; `None` retains
    /// every reachable index.
    phase_set: Option<&'a HashSet<usize>>,
}

impl<'a> DepResolver<'a> {
    pub(super) fn new(
        binding_to_idx: &'a HashMap<String, usize>,
        compositions: &[crate::resource::Composition],
        phase_set: Option<&'a HashSet<usize>>,
    ) -> Self {
        // carina#3181: composition resources are a distinct typestate, so
        // they arrive as their own slice. Index them by `binding` name —
        // a binding being present in `compositions_by_binding` *is* the
        // "this is a composition" condition the `expand` walk checks.
        let compositions_by_binding: HashMap<String, crate::resource::Composition> = compositions
            .iter()
            .filter_map(|v| v.binding.clone().map(|b| (b, v.clone())))
            .collect();
        Self {
            binding_to_idx,
            compositions_by_binding,
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

    /// Walk an effect's dependencies and merge the reached effect indices
    /// into `out`.
    ///
    /// `Effect::blocking_bindings` concentrates the blocker set: managed
    /// resources/data sources contribute value refs plus explicit
    /// `depends_on`, and waits contribute their target plus explicit
    /// wait-block dependencies.
    pub(super) fn collect_from_effect(&self, effect: &Effect, out: &mut HashSet<usize>) {
        let dep_bindings = effect.blocking_bindings();
        let mut visited: HashSet<&str> = HashSet::new();
        for binding in &dep_bindings {
            self.expand(binding.as_str(), out, &mut visited);
        }
    }

    /// Recursive dependency walk. The `'b` lifetime is bound to the
    /// `&self` borrow at the call site so the borrowed keys live
    /// inside the resolver (`compositions_by_binding` / `binding_to_idx`).
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
        // No effect for this binding. If it names a `Composition`
        // (a module's attributes-block proxy), follow the
        // references in its own attributes to the underlying
        // resources the module exposes. The typed map answers
        // "is this a composition?" by presence — no `matches!` probe.
        let Some(virt) = self.compositions_by_binding.get(binding) else {
            return;
        };
        // `get_composition_dependencies` returns owned `String`s,
        // but the visit set borrows from this resolver's keys to
        // avoid per-binding allocation. Re-borrow each inner
        // binding from the resolver's own keys so the borrow
        // lifetime matches `'b` (the `&self` borrow lifetime).
        for inner in crate::deps::get_composition_dependencies(virt) {
            let key: &'b str =
                if let Some((k, _)) = self.compositions_by_binding.get_key_value(inner.as_str()) {
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
    /// Phase 1: Wait completed.
    Wait {
        binding: String,
        outcome: WaitOutcome,
        duration: Duration,
        progress: ProgressInfo,
    },
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
    cancel: &CancellationToken,
) -> (ExecutionResult, bool) {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let (mut applied_states, wait_identifiers) = AppliedStates::with_initial(&input.current_states);
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let mut permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();
    let mut runtime_synthesized_resources: Vec<Resource> = Vec::new();

    let mut total = count_actionable_effects(input.plan.effects());
    let completed = AtomicUsize::new(0);

    let mut effects = input.plan.effects().to_vec();
    let replace_bindings = collect_replace_bindings(&effects);
    let sorted_indices = topological_sort_replaces(&effects, &replace_bindings);
    let replaced_ids: HashSet<ResourceId> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Replace { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let post_replace_wait_indices: Vec<usize> = effects
        .iter()
        .enumerate()
        .filter_map(|(idx, effect)| match effect {
            Effect::Wait { target_id, .. } if replaced_ids.contains(target_id) => Some(idx),
            _ => None,
        })
        .collect();
    let post_replace_wait_set: HashSet<usize> = post_replace_wait_indices.iter().copied().collect();
    let mut cancelled = false;

    // -----------------------------------------------------------------------
    // Phase 1: Non-Replace effects with parallel execution
    // -----------------------------------------------------------------------
    {
        let phase1_indices: Vec<usize> = (0..effects.len())
            .filter(|&idx| {
                !matches!(&effects[idx], Effect::Replace { .. } | Effect::Read { .. })
                    && is_runtime_dispatchable(&effects[idx])
                    && !matches!(
                        &effects[idx],
                        Effect::ExpandDeferredFor {
                            upstream_binding,
                            ..
                        } if replace_bindings.contains(upstream_binding)
                    )
                    && !post_replace_wait_set.contains(&idx)
            })
            .collect();

        let mut phase1_indices = phase1_indices;
        let mut deps_of = build_phase_dependency_map(
            &effects,
            &phase1_indices,
            input.unresolved_resources,
            input.compositions,
        );
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        let mut in_flight: WaitAwareInFlight<'_, PhaseEffectResult> = WaitAwareInFlight::new();

        loop {
            let undispatched_at_loop_start = phase1_indices
                .iter()
                .filter(|&&idx| !dispatched.contains(&idx))
                .count();
            if cancel.is_cancelled()
                && !cancelled
                && (undispatched_at_loop_start > 0 || !in_flight.is_empty())
            {
                cancelled = true;
                in_flight.signal_in_flight_waits();
            }

            let mut newly_ready: Vec<usize> = Vec::new();
            if !cancelled {
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
            }

            let mut completed_synchronous_dispatch = false;
            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = effects[idx].clone();

                if let Some(failed_dep) = find_failed_dependency(&effect, &failed_bindings) {
                    let c = if matches!(effect, Effect::ExpandDeferredFor { .. }) {
                        completed.load(Ordering::Relaxed)
                    } else {
                        completed.fetch_add(1, Ordering::Relaxed) + 1
                    };
                    let reason = if effect.is_wait() {
                        let detail =
                            unsatisfiable_reason_message(&UnsatisfiableReason::DependencyFailed {
                                binding: failed_dep,
                            });
                        format!("unsatisfiable: {detail}")
                    } else {
                        format!("dependency '{}' failed", failed_dep)
                    };
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect: &effect,
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

                if let Effect::ExpandDeferredFor {
                    upstream_binding,
                    template,
                    ..
                } = &effect
                {
                    let children = match expand_deferred_for_effects(
                        upstream_binding,
                        template,
                        &input.bindings,
                    ) {
                        Ok(children) => children,
                        Err(err) => {
                            let message = err.message();
                            observer.on_event(&ExecutionEvent::EffectFailed {
                                effect: &effect,
                                error: &message,
                                duration: Duration::ZERO,
                                progress: ProgressInfo {
                                    completed: completed.load(Ordering::Relaxed),
                                    total,
                                },
                            });
                            failure_count += 1;
                            failed_bindings.insert(template.binding_name.clone());
                            completed_indices.insert(idx);
                            completed_synchronous_dispatch = true;
                            break;
                        }
                    };
                    if !children.is_empty() {
                        total += count_actionable_effects(&children);
                        for child in children {
                            let child_idx = effects.len();
                            if let Effect::Create(resource) = &child {
                                runtime_synthesized_resources.push(resource.clone());
                            }
                            effects.push(child);
                            phase1_indices.push(child_idx);
                        }
                        deps_of = build_phase_dependency_map(
                            &effects,
                            &phase1_indices,
                            input.unresolved_resources,
                            input.compositions,
                        );
                    }
                    completed_indices.insert(idx);
                    completed_synchronous_dispatch = true;
                    break;
                }

                if let Effect::Wait {
                    binding,
                    target_id,
                    until,
                    timeout,
                    interval,
                    ..
                } = &effect
                {
                    let binding = binding.clone();
                    let target_id = target_id.clone();
                    let until = until.clone();
                    let timeout = *timeout;
                    let interval = *interval;
                    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let progress = ProgressInfo {
                        completed: c,
                        total,
                    };
                    let wait_identifiers = wait_identifiers.clone();
                    in_flight.push_wait(idx, |cancel_rx| {
                        let effect = effect.clone();
                        Box::pin(async move {
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted { effect: &effect });
                            let identifier_resolver = |target_id: &ResourceId| {
                                resolve_wait_identifier(&wait_identifiers, target_id)
                            };
                            let outcome = super::wait::execute_wait_effect(
                                provider,
                                &target_id,
                                &identifier_resolver,
                                &until,
                                timeout,
                                interval,
                                cancel_rx,
                                observer,
                            )
                            .await;
                            (
                                idx,
                                PhaseEffectResult::Wait {
                                    binding,
                                    outcome,
                                    duration: started.elapsed(),
                                    progress,
                                },
                            )
                        })
                    });
                    continue;
                }

                // Phase 1 only dispatches `Create`/`Update`/`Delete` to
                // `execute_basic_effect`. State-only effects (`Move`/
                // `Import`/`Remove`) are routed elsewhere (the CLI's
                // `execute_state_only_effects` step). The previous
                // `&Effect` signature let them slip through and trip an
                // `unreachable!()` (carina#3164); narrowing via
                // `as_basic()` makes the contract type-level. The
                // outer `phase1_indices` filter still excludes
                // `Replace` and `Read` so they reach their dedicated
                // phases; everything else that isn't basic ends up
                // here and is silently skipped from the basic
                // executor's point of view.
                if effect.as_basic().is_none() {
                    completed_indices.insert(idx);
                    continue;
                }

                let binding_snapshot = input.bindings.clone();
                let unresolved = &input.unresolved_resources;
                let pipeline = RenormalizePipeline {
                    normalizer: input.normalizer,
                    provider_configs: input.provider_configs,
                    factories: input.factories,
                    schemas: input.schemas,
                };
                let completed_ref = &completed;
                let effect_for_future = effect.clone();

                in_flight.push_non_wait(idx, async move {
                    let basic_effect = effect_for_future
                        .as_basic()
                        .expect("phase 1 basic dispatch must receive a basic effect");
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

            if completed_synchronous_dispatch {
                continue;
            }

            let count_undispatched =
                |dispatched: &HashSet<usize>, failed_bindings: &HashSet<String>| {
                    count_effectively_undispatched(
                        &phase1_indices,
                        dispatched,
                        &effects,
                        failed_bindings,
                    )
                };
            in_flight
                .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                .cancel_if_terminal()
                .drop_without_awaiting();

            if in_flight.is_empty() {
                if cancelled {
                    emit_cancelled_skips_with_progress(
                        &effects,
                        &phase1_indices,
                        &mut dispatched,
                        &mut completed_indices,
                        &mut skip_count,
                        observer,
                        |_| ProgressInfo {
                            completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
                            total,
                        },
                    );
                }
                break;
            }

            let (finished_idx, result) = if cancelled {
                let Some(finished) = in_flight
                    .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                    .cancel_if_terminal()
                    .next_completed()
                    .await
                else {
                    break;
                };
                finished
            } else {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        in_flight.signal_in_flight_waits();
                        continue;
                    }
                    finished = in_flight
                        .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                        .cancel_if_terminal()
                        .next_completed() => {
                        let Some(finished) = finished else {
                            break;
                        };
                        finished
                    }
                }
            };
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
                PhaseEffectResult::Wait {
                    binding,
                    outcome,
                    duration,
                    progress,
                } => match outcome {
                    WaitOutcome::Satisfied { state } => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect: &effects[finished_idx],
                            state: Some(&state),
                            duration,
                            progress,
                        });
                        success_count += 1;
                        let synthetic = ResourceId::new("__wait", &binding);
                        let attrs: HashMap<String, Value> = state
                            .attributes
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect();
                        input
                            .bindings
                            .record_applied(Some(&binding), &attrs, &state);
                        applied_states.insert(synthetic, state);
                    }
                    WaitOutcome::Unsatisfiable(reason) => {
                        let detail = unsatisfiable_reason_message(&reason);
                        let reason = format!("unsatisfiable: {detail}");
                        observer.on_event(&ExecutionEvent::EffectSkipped {
                            effect: &effects[finished_idx],
                            reason: &reason,
                            progress,
                        });
                        skip_count += 1;
                        failed_bindings.insert(binding);
                    }
                    WaitOutcome::Cancelled => {
                        observer.on_event(&ExecutionEvent::EffectSkipped {
                            effect: &effects[finished_idx],
                            reason: SKIP_REASON_CANCELLED,
                            progress,
                        });
                        skip_count += 1;
                    }
                    outcome @ (WaitOutcome::Timeout { .. }
                    | WaitOutcome::NotFound(_)
                    | WaitOutcome::ReadFailed(_)) => {
                        let error =
                            wait_failure_message(&outcome, effects[finished_idx].resource_id());
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect: &effects[finished_idx],
                            error: &error,
                            duration,
                            progress,
                        });
                        failure_count += 1;
                        failed_bindings.insert(binding);
                    }
                },
                _ => unreachable!(),
            }
            in_flight
                .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                .cancel_if_terminal()
                .drop_without_awaiting();
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

        let deps_of = build_phase_dependency_map(
            &effects,
            &cbd_indices,
            input.unresolved_resources,
            input.compositions,
        );
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        let mut in_flight = FuturesUnordered::new();

        loop {
            let undispatched_at_loop_start = cbd_indices
                .iter()
                .filter(|&&idx| !dispatched.contains(&idx))
                .count();
            if cancel.is_cancelled()
                && !cancelled
                && (undispatched_at_loop_start > 0 || !in_flight.is_empty())
            {
                cancelled = true;
            }

            let mut newly_ready: Vec<usize> = Vec::new();
            if !cancelled {
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
            }

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
                    provider_configs: input.provider_configs,
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

                        let resolve_source = unresolved
                            .get(&to.id)
                            .map_or(to, UnresolvedResource::as_resource);
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

                        let resolved_attrs = resolved.as_resource().resolved_attributes();
                        match provider
                            .create(&to.id, CreateRequest { resource: resolved })
                            .await
                        {
                            Ok(state) => {
                                let mut local_bindings = binding_snapshot.clone();
                                local_bindings.record_applied(
                                    to.binding.as_deref(),
                                    &resolved_attrs,
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
                                    let cascade_patch = compute_full_diff_patch(
                                        &cascade.from,
                                        &resolved_to,
                                        &cascade.to,
                                        pipeline.schemas,
                                        &cascade.id,
                                    );
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
                                            let cascade_attrs =
                                                resolved_to.as_resource().resolved_attributes();
                                            local_bindings.record_applied(
                                                cascade.to.binding.as_deref(),
                                                &cascade_attrs,
                                                &cascade_state,
                                            );
                                            cascade_states.push((
                                                cascade.id.clone(),
                                                cascade_state,
                                                cascade_attrs,
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
                if cancelled {
                    emit_cancelled_skips_with_progress(
                        &effects,
                        &cbd_indices,
                        &mut dispatched,
                        &mut completed_indices,
                        &mut skip_count,
                        observer,
                        |idx| replace_progress[&idx],
                    );
                }
                break;
            }

            let (finished_idx, started, result) = if cancelled {
                in_flight.next().await.unwrap()
            } else {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        continue;
                    }
                    finished = in_flight.next() => finished.unwrap(),
                }
            };
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
    // Phase 3: All deletes with parallel execution (reverse dependency order)
    // -----------------------------------------------------------------------
    if !cancelled {
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
        let resolver = DepResolver::new(&binding_to_idx, input.compositions, Some(&phase_set));
        let mut deps_of: HashMap<usize, HashSet<usize>> = HashMap::new();
        for &idx in &delete_indices {
            let effect = &effects[idx];
            let mut dep_indices = HashSet::new();
            if effect.as_resource_ref().is_some() {
                resolver.collect_from_effect(effect, &mut dep_indices);
                if let Some(unresolved) = input.unresolved_resources.get(effect.resource_id()) {
                    resolver.collect_from_resource(unresolved.as_resource(), &mut dep_indices);
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
            let undispatched_at_loop_start = delete_indices
                .iter()
                .filter(|&&idx| !dispatched.contains(&idx))
                .count();
            if cancel.is_cancelled()
                && !cancelled
                && (undispatched_at_loop_start > 0 || !in_flight.is_empty())
            {
                cancelled = true;
            }

            let mut newly_ready: Vec<usize> = Vec::new();
            if !cancelled {
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
            }

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
                if cancelled {
                    emit_cancelled_skips_with_progress(
                        &effects,
                        &delete_indices,
                        &mut dispatched,
                        &mut completed_indices,
                        &mut skip_count,
                        observer,
                        |idx| replace_progress[&idx],
                    );
                }
                break;
            }

            let (finished_idx, result) = if cancelled {
                in_flight.next().await.unwrap()
            } else {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        continue;
                    }
                    finished = in_flight.next() => finished.unwrap(),
                }
            };
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
    if !cancelled {
        let mut phase4_indices: Vec<usize> = sorted_indices.clone();
        phase4_indices.extend(effects.iter().enumerate().filter_map(
            |(idx, effect)| match effect {
                Effect::ExpandDeferredFor {
                    upstream_binding, ..
                } if replace_bindings.contains(upstream_binding) => Some(idx),
                Effect::Create(_)
                | Effect::Update { .. }
                | Effect::Replace { .. }
                | Effect::Delete { .. }
                | Effect::Read { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::Wait { .. }
                | Effect::ExpandDeferredFor { .. } => None,
            },
        ));
        phase4_indices.sort();

        let mut deps_of = build_phase_dependency_map(
            &effects,
            &phase4_indices,
            input.unresolved_resources,
            input.compositions,
        );
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        type PhaseFuture<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (usize, PhaseEffectResult)> + 'a>>;
        // Phase 4 dispatches only Replace finalize/create work from sorted
        // Replace indices; no Wait effects can appear here, so no wait_cancellers
        // are needed for cancellation.
        let mut in_flight: FuturesUnordered<PhaseFuture<'_>> = FuturesUnordered::new();

        loop {
            let undispatched_at_loop_start = phase4_indices
                .iter()
                .filter(|&&idx| !dispatched.contains(&idx))
                .count();
            if cancel.is_cancelled()
                && !cancelled
                && (undispatched_at_loop_start > 0 || !in_flight.is_empty())
            {
                cancelled = true;
            }

            let mut newly_ready: Vec<usize> = Vec::new();
            if !cancelled {
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
            }

            let mut completed_synchronous_dispatch = false;
            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = effects[idx].clone();

                if let Some(failed_dep) = find_failed_dependency(&effect, &failed_bindings) {
                    let reason = format!("dependency '{}' failed", failed_dep);
                    let progress =
                        replace_progress
                            .get(&idx)
                            .copied()
                            .unwrap_or_else(|| ProgressInfo {
                                completed: completed.load(Ordering::Relaxed),
                                total,
                            });
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect: &effect,
                        reason: &reason,
                        progress,
                    });
                    if let Some(binding) = effect.binding_name() {
                        failed_bindings.insert(binding);
                    }
                    completed_indices.insert(idx);
                    continue;
                }

                if let Effect::ExpandDeferredFor {
                    upstream_binding,
                    template,
                    ..
                } = &effect
                {
                    let children = match expand_deferred_for_effects(
                        upstream_binding,
                        template,
                        &input.bindings,
                    ) {
                        Ok(children) => children,
                        Err(err) => {
                            let message = err.message();
                            observer.on_event(&ExecutionEvent::EffectFailed {
                                effect: &effect,
                                error: &message,
                                duration: Duration::ZERO,
                                progress: ProgressInfo {
                                    completed: completed.load(Ordering::Relaxed),
                                    total,
                                },
                            });
                            failure_count += 1;
                            failed_bindings.insert(template.binding_name.clone());
                            completed_indices.insert(idx);
                            completed_synchronous_dispatch = true;
                            break;
                        }
                    };
                    if !children.is_empty() {
                        total += count_actionable_effects(&children);
                        for child in children {
                            let child_idx = effects.len();
                            if let Effect::Create(resource) = &child {
                                runtime_synthesized_resources.push(resource.clone());
                                debug_assert!(
                                    resource.dependency_bindings.contains(upstream_binding),
                                    "apply-time deferred-for child must retain iterable dependency"
                                );
                            }
                            effects.push(child);
                            phase4_indices.push(child_idx);
                        }
                        deps_of = build_phase_dependency_map(
                            &effects,
                            &phase4_indices,
                            input.unresolved_resources,
                            input.compositions,
                        );
                    }
                    completed_indices.insert(idx);
                    completed_synchronous_dispatch = true;
                    break;
                }

                if effect.as_basic().is_some() {
                    let binding_snapshot = input.bindings.clone();
                    let unresolved = &input.unresolved_resources;
                    let pipeline = RenormalizePipeline {
                        normalizer: input.normalizer,
                        provider_configs: input.provider_configs,
                        factories: input.factories,
                        schemas: input.schemas,
                    };
                    let completed_ref = &completed;
                    let effect_for_future = effect.clone();
                    in_flight.push(Box::pin(async move {
                        let basic_effect = effect_for_future
                            .as_basic()
                            .expect("dynamic phase 4 create must be a basic effect");
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
                    }));
                    continue;
                }

                let binding_snapshot = input.bindings.clone();
                let unresolved = &input.unresolved_resources;
                let pipeline = RenormalizePipeline {
                    normalizer: input.normalizer,
                    provider_configs: input.provider_configs,
                    factories: input.factories,
                    schemas: input.schemas,
                };

                if let Effect::Replace {
                    id,
                    to,
                    directives,
                    temporary_name,
                    ..
                } = &effect
                {
                    let progress = replace_progress[&idx];
                    let effect_started = replace_start_times
                        .get(&idx)
                        .copied()
                        .unwrap_or_else(Instant::now);

                    if directives.create_before_destroy {
                        // CBD finalization: skip if create phase failed
                        let Some(state) = cbd_create_states.get(&idx).cloned() else {
                            completed_indices.insert(idx);
                            continue;
                        };
                        let id = id.clone();
                        let to = to.clone();
                        let temporary_name = temporary_name.clone();
                        let effect_for_future = effect.clone();

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
                                            effect: &effect_for_future,
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
                                            effect: &effect_for_future,
                                            error: "rename failed",
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::CbdFinalizeFailed {
                                                state,
                                                resource_id: to.id.clone(),
                                                binding: effect_for_future.binding_name(),
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
                                    effect: &effect_for_future,
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
                        let effect_for_future = effect.clone();
                        in_flight.push(Box::pin(async move {
                            if let Effect::Replace { to, .. } = &effect_for_future {
                                let started = effect_started;
                                let resolve_source = unresolved
                                    .get(&to.id)
                                    .map_or(to, UnresolvedResource::as_resource);
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
                                            effect: &effect_for_future,
                                            error: &e,
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        return (
                                            idx,
                                            PhaseEffectResult::Basic(BasicEffectResult::Failure {
                                                binding: effect_for_future.binding_name(),
                                                refresh: None,
                                            }),
                                        );
                                    }
                                };

                                let resolved_attrs = resolved.as_resource().resolved_attributes();
                                match provider
                                    .create(&to.id, CreateRequest { resource: resolved })
                                    .await
                                {
                                    Ok(state) => {
                                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                                            effect: &effect_for_future,
                                            state: Some(&state),
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::NonCbdCreateSuccess {
                                                state,
                                                resource_id: to.id.clone(),
                                                resolved_attrs,
                                                binding: to.binding.clone(),
                                            },
                                        )
                                    }
                                    Err(e) => {
                                        let error_str = e.to_string();
                                        observer.on_event(&ExecutionEvent::EffectFailed {
                                            effect: &effect_for_future,
                                            error: &error_str,
                                            duration: started.elapsed(),
                                            progress,
                                        });
                                        (
                                            idx,
                                            PhaseEffectResult::NonCbdCreateFailure {
                                                binding: effect_for_future.binding_name(),
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

            if completed_synchronous_dispatch {
                continue;
            }

            if in_flight.is_empty() {
                if cancelled {
                    emit_cancelled_skips_with_progress(
                        &effects,
                        &phase4_indices,
                        &mut dispatched,
                        &mut completed_indices,
                        &mut skip_count,
                        observer,
                        |idx| {
                            replace_progress
                                .get(&idx)
                                .copied()
                                .unwrap_or_else(|| ProgressInfo {
                                    completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
                                    total,
                                })
                        },
                    );
                }
                break;
            }

            let (finished_idx, result) = if cancelled {
                in_flight.next().await.unwrap()
            } else {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        continue;
                    }
                    finished = in_flight.next() => finished.unwrap(),
                }
            };
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::CbdFinalizeSuccess {
                    state,
                    resource_id,
                    permanent_overrides,
                } => {
                    cbd_create_states.remove(&finished_idx);
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
                    cbd_create_states.remove(&finished_idx);
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
    // Phase 5: Waits whose targets were replaced in this phased run
    // -----------------------------------------------------------------------
    if !cancelled && !post_replace_wait_indices.is_empty() {
        let deps_of = build_phase_dependency_map(
            &effects,
            &post_replace_wait_indices,
            input.unresolved_resources,
            input.compositions,
        );
        let mut completed_indices: HashSet<usize> = HashSet::new();
        let mut dispatched: HashSet<usize> = HashSet::new();
        let mut in_flight: WaitAwareInFlight<'_, PhaseEffectResult> = WaitAwareInFlight::new();

        loop {
            let mut newly_ready: Vec<usize> = Vec::new();
            if !cancelled {
                for &idx in &post_replace_wait_indices {
                    if dispatched.contains(&idx) {
                        continue;
                    }
                    let deps = deps_of.get(&idx).cloned().unwrap_or_default();
                    if deps.iter().all(|d| completed_indices.contains(d)) {
                        newly_ready.push(idx);
                    } else {
                        let pending: Vec<String> = deps
                            .iter()
                            .filter(|d| !completed_indices.contains(d))
                            .map(|d| effects[*d].resource_id().to_string())
                            .collect();
                        observer.on_event(&ExecutionEvent::Waiting {
                            effect: &effects[idx],
                            pending_dependencies: pending,
                        });
                    }
                }
                newly_ready.sort();
            }

            for idx in newly_ready {
                dispatched.insert(idx);
                let effect = &effects[idx];

                if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
                    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let detail =
                        unsatisfiable_reason_message(&UnsatisfiableReason::DependencyFailed {
                            binding: failed_dep,
                        });
                    let reason = format!("unsatisfiable: {detail}");
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

                if let Effect::Wait {
                    binding,
                    target_id,
                    until,
                    timeout,
                    interval,
                    ..
                } = effect
                {
                    let c = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let progress = ProgressInfo {
                        completed: c,
                        total,
                    };
                    let wait_identifiers = wait_identifiers.clone();
                    in_flight.push_wait(idx, |cancel_rx| {
                        Box::pin(async move {
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted { effect });
                            let identifier_resolver = |target_id: &ResourceId| {
                                resolve_wait_identifier(&wait_identifiers, target_id)
                            };
                            let outcome = super::wait::execute_wait_effect(
                                provider,
                                target_id,
                                &identifier_resolver,
                                until,
                                *timeout,
                                *interval,
                                cancel_rx,
                                observer,
                            )
                            .await;
                            (
                                idx,
                                PhaseEffectResult::Wait {
                                    binding: binding.clone(),
                                    outcome,
                                    duration: started.elapsed(),
                                    progress,
                                },
                            )
                        })
                    });
                }
            }

            let count_undispatched =
                |dispatched: &HashSet<usize>, failed_bindings: &HashSet<String>| {
                    count_effectively_undispatched(
                        &post_replace_wait_indices,
                        dispatched,
                        &effects,
                        failed_bindings,
                    )
                };
            in_flight
                .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                .cancel_if_terminal()
                .drop_without_awaiting();

            if in_flight.is_empty() {
                if cancelled {
                    emit_cancelled_skips_with_progress(
                        &effects,
                        &post_replace_wait_indices,
                        &mut dispatched,
                        &mut completed_indices,
                        &mut skip_count,
                        observer,
                        |_| ProgressInfo {
                            completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
                            total,
                        },
                    );
                }
                break;
            }

            let (finished_idx, result) = if cancelled {
                let Some(finished) = in_flight
                    .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                    .cancel_if_terminal()
                    .next_completed()
                    .await
                else {
                    break;
                };
                finished
            } else {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        in_flight.signal_in_flight_waits();
                        continue;
                    }
                    finished = in_flight
                        .check_terminal(count_undispatched(&dispatched, &failed_bindings))
                        .cancel_if_terminal()
                        .next_completed() => {
                        let Some(finished) = finished else {
                            break;
                        };
                        finished
                    }
                }
            };
            completed_indices.insert(finished_idx);

            match result {
                PhaseEffectResult::Wait {
                    binding,
                    outcome,
                    duration,
                    progress,
                } => match outcome {
                    WaitOutcome::Satisfied { state } => {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect: &effects[finished_idx],
                            state: Some(&state),
                            duration,
                            progress,
                        });
                        success_count += 1;
                        let synthetic = ResourceId::new("__wait", &binding);
                        let attrs: HashMap<String, Value> = state
                            .attributes
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect();
                        input
                            .bindings
                            .record_applied(Some(&binding), &attrs, &state);
                        applied_states.insert(synthetic, state);
                    }
                    WaitOutcome::Unsatisfiable(reason) => {
                        let detail = unsatisfiable_reason_message(&reason);
                        let reason = format!("unsatisfiable: {detail}");
                        observer.on_event(&ExecutionEvent::EffectSkipped {
                            effect: &effects[finished_idx],
                            reason: &reason,
                            progress,
                        });
                        skip_count += 1;
                        failed_bindings.insert(binding);
                    }
                    WaitOutcome::Cancelled => {
                        observer.on_event(&ExecutionEvent::EffectSkipped {
                            effect: &effects[finished_idx],
                            reason: SKIP_REASON_CANCELLED,
                            progress,
                        });
                        skip_count += 1;
                    }
                    outcome @ (WaitOutcome::Timeout { .. }
                    | WaitOutcome::NotFound(_)
                    | WaitOutcome::ReadFailed(_)) => {
                        let error =
                            wait_failure_message(&outcome, effects[finished_idx].resource_id());
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect: &effects[finished_idx],
                            error: &error,
                            duration,
                            progress,
                        });
                        failure_count += 1;
                        failed_bindings.insert(binding);
                    }
                },
                _ => unreachable!(),
            }
        }
    }

    // Preserve CBD create states for any temporary that was created in Phase 2
    // but did not complete finalize. Phase 4 removes finalized indices from
    // cbd_create_states, so anything remaining here is genuinely unprocessed.
    // Do not re-call bindings.record_applied: Phase 2 already recorded the
    // binding when the create succeeded, matching Phase 4's success path.
    if cancelled {
        for (idx, state) in cbd_create_states {
            if let Effect::Replace { to, .. } = &effects[idx] {
                success_count += 1;
                applied_states.insert(to.id.clone(), state);
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

    let result = ExecutionResult {
        success_count,
        failure_count,
        skip_count,
        applied_states: applied_states.into_inner(),
        runtime_synthesized_resources,
        successfully_deleted,
        permanent_name_overrides,
        current_states: input.current_states.clone(),
        bindings: input.bindings.clone(),
        failed_refreshes,
    };
    (result, cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Plan;
    use crate::provider::{
        BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, ProviderError, ProviderResult,
        ReadRequest, UpdateRequest,
    };
    use crate::resource::{Composition, ConcreteValue, DataSource, ResourceId, Value};
    use crate::schema::SchemaRegistry;
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::sync::Mutex;

    struct TerminalWaitProvider {
        create_log: Mutex<Vec<String>>,
    }

    impl TerminalWaitProvider {
        fn new() -> Self {
            Self {
                create_log: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for TerminalWaitProvider {
        fn name(&self) -> &str {
            "terminal-wait"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let mut attrs = HashMap::new();
            attrs.insert(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
            );
            let state = State::existing(id.clone(), attrs).with_identifier("cert-id");
            Box::pin(async move { Ok(state) })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            self.read(&resource.id, None, ReadRequest)
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.create_log
                .lock()
                .unwrap()
                .push(id.name_str().to_string());
            let id = id.clone();
            Box::pin(async move {
                if id.name_str() == "alb" {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Err(ProviderError::api_error("alb create failed"))
                } else {
                    Ok(State::existing(id, HashMap::new()).with_identifier("cert-id"))
                }
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("update not used")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("delete not used")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    struct RecordingSkipObserver {
        skipped: Mutex<Vec<String>>,
        failures: Mutex<Vec<String>>,
    }

    impl RecordingSkipObserver {
        fn new() -> Self {
            Self {
                skipped: Mutex::new(Vec::new()),
                failures: Mutex::new(Vec::new()),
            }
        }

        fn skipped(&self) -> Vec<String> {
            self.skipped.lock().unwrap().clone()
        }

        fn failures(&self) -> Vec<String> {
            self.failures.lock().unwrap().clone()
        }
    }

    impl ExecutionObserver for RecordingSkipObserver {
        fn on_event(&self, event: &ExecutionEvent) {
            match event {
                ExecutionEvent::EffectSkipped { effect, reason, .. } => {
                    self.skipped
                        .lock()
                        .unwrap()
                        .push(format!("{}:{reason}", effect.resource_id()));
                }
                ExecutionEvent::EffectFailed { effect, error, .. } => {
                    self.failures
                        .lock()
                        .unwrap()
                        .push(format!("{}:{error}", effect.resource_id()));
                }
                ExecutionEvent::Waiting { .. }
                | ExecutionEvent::EffectStarted { .. }
                | ExecutionEvent::EffectSucceeded { .. }
                | ExecutionEvent::WaitPolling { .. }
                | ExecutionEvent::CascadeUpdateSucceeded { .. }
                | ExecutionEvent::CascadeUpdateFailed { .. }
                | ExecutionEvent::RenameSucceeded { .. }
                | ExecutionEvent::RenameFailed { .. }
                | ExecutionEvent::RefreshStarted
                | ExecutionEvent::RefreshSucceeded { .. }
                | ExecutionEvent::RefreshFailed { .. } => {}
            }
        }
    }

    /// Reproduces #2543: when a resource depends on `<module-instance>.<attr>`
    /// (where the module-instance binding is a `Virtual` resource exposing the
    /// module's `attributes { }`), the executor's phase dependency map drops
    /// the dep silently — composition resources have no Effect entry to look up.
    /// The fix must follow the composition binding through to the underlying
    /// resource(s) it references.
    /// Build a [`Composition`] with a single `ResourceRef` attribute.
    fn make_virtual(
        id_name: &str,
        binding: &str,
        attr: &str,
        ref_binding: &str,
        ref_attr: &str,
    ) -> Composition {
        let mut attributes = indexmap::IndexMap::new();
        attributes.insert(
            attr.to_string(),
            crate::resource::CompositionAttribute::from_value(Value::resource_ref(
                ref_binding,
                ref_attr,
                vec![],
            )),
        );
        Composition {
            id: ResourceId::with_provider("_virtual", "_virtual", id_name, None),
            signature: crate::resource::Signature {
                arguments: indexmap::IndexMap::new(),
                attributes,
            },
            binding: Some(binding.to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "mod".to_string(),
            instance: binding.to_string(),
            quoted_string_attrs: std::collections::HashSet::new(),
        }
    }

    #[tokio::test]
    async fn phased_wait_marked_unsatisfiable_when_only_waits_in_flight() {
        let provider = TerminalWaitProvider::new();

        let mut cert = Resource::new("test", "cert");
        cert.binding = Some("cert".to_string());
        let cert_id = cert.id.clone();

        let mut alb = Resource::new("test", "alb");
        alb.binding = Some("alb".to_string());

        let mut plan = Plan::new();
        plan.add(Effect::Create(cert.clone()));
        plan.add(Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: cert_id.clone(),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_millis(1),
            explicit_dependencies: std::collections::HashSet::new(),
        });
        plan.add(Effect::Create(alb.clone()));

        let unresolved = HashMap::from([
            (
                cert.id.clone(),
                UnresolvedResource::from_pre_resolve(cert.clone()),
            ),
            (
                alb.id.clone(),
                UnresolvedResource::from_pre_resolve(alb.clone()),
            ),
        ]);
        let schemas = SchemaRegistry::new();
        let mut input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &unresolved,
            compositions: &[],
            bindings: Default::default(),
            current_states: HashMap::new(),
            normalizer: &NoopNormalizer,
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
            parallelism: std::num::NonZeroUsize::new(2).unwrap(),
        };
        let observer = RecordingSkipObserver::new();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            execute_effects_phased(&provider, &mut input, &observer, &CancellationToken::new()),
        )
        .await
        .expect("wait should be skipped as unsatisfiable instead of timing out");
        let (result, was_cancelled) = result;
        assert!(!was_cancelled);

        assert!(
            result.failure_count >= 1,
            "alb create should fail in phased execution"
        );
        assert_eq!(
            result.skip_count, 1,
            "wait should be skipped when only waits remain in flight"
        );
        assert!(
            observer
                .failures()
                .iter()
                .any(|event| event.contains("alb")),
            "test setup should fail alb create; failures: {:?}",
            observer.failures()
        );
        assert!(
            observer.skipped().iter().any(|event| {
                event.contains("cert") && event.to_ascii_lowercase().contains("unsatisfiable")
            }),
            "wait skip reason should contain unsatisfiable, skipped: {:?}",
            observer.skipped()
        );
    }

    #[tokio::test]
    async fn phased_wait_marked_unsatisfiable_when_failing_sibling_blocks_consumer_inside_wait_subtree()
     {
        let provider = TerminalWaitProvider::new();

        let mut cert = Resource::new("test", "cert");
        cert.binding = Some("cert".to_string());
        let cert_id = cert.id.clone();

        let mut alb = Resource::new("test", "alb");
        alb.binding = Some("alb".to_string());

        let mut listener = Resource::new("test", "listener");
        listener.binding = Some("listener".to_string());
        listener.set_attr(
            "load_balancer_arn",
            Value::resource_ref("alb", "load_balancer_arn", vec![]),
        );
        listener.set_attr(
            "certificate_arn",
            Value::resource_ref("cert_issued", "arn", vec![]),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Create(cert.clone()));
        plan.add(Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: cert_id.clone(),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_millis(1),
            explicit_dependencies: std::collections::HashSet::new(),
        });
        plan.add(Effect::Create(alb.clone()));
        plan.add(Effect::Create(listener.clone()));

        let unresolved = HashMap::from([
            (
                cert.id.clone(),
                UnresolvedResource::from_pre_resolve(cert.clone()),
            ),
            (
                alb.id.clone(),
                UnresolvedResource::from_pre_resolve(alb.clone()),
            ),
            (
                listener.id.clone(),
                UnresolvedResource::from_pre_resolve(listener.clone()),
            ),
        ]);
        let phase1_indices: Vec<usize> = (0..plan.effects().len()).collect();
        let deps = build_phase_dependency_map(plan.effects(), &phase1_indices, &unresolved, &[]);
        assert!(
            deps.get(&3).is_some_and(|listener_deps| {
                listener_deps.contains(&1) && listener_deps.contains(&2)
            }),
            "listener should depend on both alb and cert_issued; deps: {deps:?}"
        );

        let schemas = SchemaRegistry::new();
        let mut input = ExecutionInput {
            plan: &plan,
            unresolved_resources: &unresolved,
            compositions: &[],
            bindings: Default::default(),
            current_states: HashMap::new(),
            normalizer: &NoopNormalizer,
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
            parallelism: std::num::NonZeroUsize::new(2).unwrap(),
        };
        let observer = RecordingSkipObserver::new();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            execute_effects_phased(&provider, &mut input, &observer, &CancellationToken::new()),
        )
        .await
        .expect("wait should be skipped as unsatisfiable instead of polling until timeout");
        let (result, was_cancelled) = result;
        assert!(!was_cancelled);

        assert!(
            result.failure_count >= 1,
            "alb create should fail; failure_count: {}",
            result.failure_count
        );
        assert!(
            result.skip_count >= 1,
            "listener and cert_issued should be skipped; skip_count: {}",
            result.skip_count
        );
        assert!(
            observer.skipped().iter().any(|event| {
                event.contains("cert") && event.to_ascii_lowercase().contains("unsatisfiable")
            }),
            "wait skip reason should contain unsatisfiable, skipped: {:?}",
            observer.skipped()
        );
    }

    #[test]
    fn build_phase_dependency_map_follows_virtual_module_binding() {
        let mut role = Resource::with_provider("awscc", "iam.Role", "bootstrap.role", None);
        role.binding = Some("bootstrap.role".to_string());

        // carina#3181: the composition exposes `role_name = role.role_name`,
        // which after intra-module rewriting refs `bootstrap.role`.
        let virt = make_virtual(
            "bootstrap",
            "bootstrap",
            "role_name",
            "bootstrap.role",
            "role_name",
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

        let mut unresolved: HashMap<ResourceId, UnresolvedResource> = HashMap::new();
        unresolved.insert(
            role.id.clone(),
            UnresolvedResource::from_pre_resolve(role.clone()),
        );
        unresolved.insert(
            role_policy.id.clone(),
            UnresolvedResource::from_pre_resolve(role_policy.clone()),
        );

        let deps_of = build_phase_dependency_map(&effects, &phase_indices, &unresolved, &[virt]);

        assert!(
            deps_of[&1].contains(&0),
            "RolePolicy (idx 1) must depend on Role (idx 0) via the bootstrap composition binding; got: {:?}",
            deps_of[&1],
        );
    }

    /// Module nesting: the outer caller references a composition binding whose own
    /// attribute references another composition binding. The dep walk must drill
    /// through both layers to the underlying resource.
    #[test]
    fn build_phase_dependency_map_follows_nested_virtual_module_bindings() {
        let mut role = Resource::with_provider("awscc", "iam.Role", "outer.inner.role", None);
        role.binding = Some("outer.inner.role".to_string());

        let inner_virt = make_virtual(
            "outer.inner",
            "outer.inner",
            "role_name",
            "outer.inner.role",
            "role_name",
        );
        let outer_virt = make_virtual("outer", "outer", "role_name", "outer.inner", "role_name");

        let mut caller = Resource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        caller.set_attr(
            "role_name",
            Value::resource_ref("outer", "role_name", vec![]),
        );

        let effects = vec![Effect::Create(role.clone()), Effect::Create(caller.clone())];
        let phase_indices: Vec<usize> = vec![0, 1];

        let mut unresolved: HashMap<ResourceId, UnresolvedResource> = HashMap::new();
        unresolved.insert(role.id.clone(), UnresolvedResource::from_pre_resolve(role));
        unresolved.insert(
            caller.id.clone(),
            UnresolvedResource::from_pre_resolve(caller),
        );

        let deps_of = build_phase_dependency_map(
            &effects,
            &phase_indices,
            &unresolved,
            &[inner_virt, outer_virt],
        );

        assert!(
            deps_of[&1].contains(&0),
            "caller must depend on Role through two composition layers (outer → outer.inner → outer.inner.role); got: {:?}",
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
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec![
                "role_name".to_string(),
            ])
            .unwrap(),
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };
        let bucket_replace = Effect::Replace {
            id: bucket.id.clone(),
            from: Box::new(bucket_state),
            to: bucket.clone(),
            directives: Directives::default(),
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec![
                "bucket_name".to_string(),
            ])
            .unwrap(),
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
