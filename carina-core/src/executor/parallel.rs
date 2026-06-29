//! Dependency computation and parallel effect execution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::effect::Effect;
#[cfg(test)]
use crate::effect::deps::UnresolvedResource;
use crate::provider::Provider;
use crate::resource::{Resource, ResourceId, Value};

use super::basic::{
    BasicEffectCtx, ExecutionState, RenormalizePipeline, count_actionable_effects,
    execute_basic_effect, process_basic_result, refresh_pending_states,
};
use super::deferred_dispatch::PureMetaCtx;
use super::replace::SingleEffectResult;
use super::scheduler::{
    FailureView, PureMetaOutcome, build_scheduler_deps, dependency_failed_reason,
    emit_cancelled_skips_with_progress, failure_binding_name, try_dispatch_pure_meta,
    wait_dependency_failed_reason,
};
use super::wait::{
    AppliedStates, SKIP_REASON_CANCELLED, WaitAwareInFlight, WaitOutcome, WaitSignal,
    count_effectively_undispatched, resolve_wait_identifier, unsatisfiable_reason_message,
    wait_failure_message,
};
use super::{ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo};

pub(super) struct ExpandedEffects {
    pub(super) effects: Vec<Effect>,
    pub(super) deferred_replace_delete_deps: Vec<(usize, usize)>,
}

pub(super) fn expand_deferred_replace_effects(plan_effects: &[Effect]) -> ExpandedEffects {
    let mut effects = Vec::with_capacity(plan_effects.len());
    let mut deferred_replace_delete_deps = Vec::new();

    for effect in plan_effects {
        if let Effect::DeferredReplace { deletes, .. } = effect {
            let mut delete_indices = Vec::with_capacity(deletes.len());
            for delete in deletes.iter() {
                let delete_idx = effects.len();
                effects.push(delete.to_delete_effect());
                delete_indices.push(delete_idx);
            }
            let gate_idx = effects.len();
            effects.push(effect.clone());
            deferred_replace_delete_deps.extend(
                delete_indices
                    .into_iter()
                    .map(|delete_idx| (gate_idx, delete_idx)),
            );
        } else {
            effects.push(effect.clone());
        }
    }

    ExpandedEffects {
        effects,
        deferred_replace_delete_deps,
    }
}

pub(super) fn apply_deferred_replace_delete_deps(
    deps_of: &mut HashMap<usize, HashSet<usize>>,
    deferred_replace_delete_deps: &[(usize, usize)],
) {
    for &(gate_idx, delete_idx) in deferred_replace_delete_deps {
        if deps_of.contains_key(&gate_idx) {
            deps_of.entry(gate_idx).or_default().insert(delete_idx);
        }
    }
}

pub(super) fn is_runtime_dispatchable(effect: &Effect) -> bool {
    !matches!(effect, Effect::Read { .. })
        && (!effect.is_state_operation() || effect.is_scheduler_meta())
}

pub(super) fn is_runtime_noop(effect: &Effect) -> bool {
    matches!(effect, Effect::Read { .. })
        || (effect.is_state_operation() && !effect.is_scheduler_meta())
}

#[cfg(test)]
pub(super) fn build_dependency_levels(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
) -> Vec<Vec<usize>> {
    let deps_of = crate::effect::deps::build_effect_dependency_analysis(
        effects,
        unresolved_resources,
        compositions,
        crate::effect::deps::ScheduleInputs::Apply,
    )
    .into_deps_of();

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
    cancel: &CancellationToken,
) -> (ExecutionResult, bool) {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut partial_count = 0;
    let mut partial_diagnostics = Vec::new();
    let mut skip_count = 0;
    let (mut applied_states, wait_identifiers) = AppliedStates::with_initial(&input.current_states);
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();
    let mut runtime_synthesized_resources: Vec<Resource> = Vec::new();

    let ExpandedEffects {
        effects: expanded_effects,
        deferred_replace_delete_deps,
    } = expand_deferred_replace_effects(input.plan.effects());
    let mut effects = expanded_effects;
    let mut total = count_actionable_effects(&effects);
    let completed = AtomicUsize::new(0);

    let mut deps_of = build_scheduler_deps(
        &effects,
        input.unresolved_resources,
        input.compositions,
        &deferred_replace_delete_deps,
    );

    // Build effect index -> binding name mapping for resolving dependency names
    let mut idx_to_binding: HashMap<usize, String> = effects
        .iter()
        .enumerate()
        .filter_map(|(idx, effect)| failure_binding_name(effect).map(|b| (idx, b)))
        .collect();

    // Track which effect indices have completed (successfully or not)
    let mut completed_indices: HashSet<usize> = HashSet::new();
    // Track effect indices whose failure should poison direct scheduler dependents.
    let mut failed_indices: HashSet<usize> = HashSet::new();
    // Track which effect indices have been dispatched (spawned or skipped)
    let mut dispatched: HashSet<usize> = HashSet::new();
    // All actionable effect indices (excluding Read and state operations)
    let mut actionable_indices: Vec<usize> = (0..effects.len())
        .filter(|&idx| is_runtime_dispatchable(&effects[idx]))
        .collect();

    // Mark Read and plain state operation effects as completed (they are no-ops in the executor).
    // DeferredCreate is state-only for progress/provider purposes, but it is a scheduler
    // dispatch point that materializes dynamic Create effects.
    for (idx, effect) in effects.iter().enumerate() {
        if is_runtime_noop(effect) {
            completed_indices.insert(idx);
            dispatched.insert(idx);
        }
    }

    let mut in_flight: WaitAwareInFlight<'_, SingleEffectResult> = WaitAwareInFlight::new();
    let mut cancelled = false;

    loop {
        let undispatched_at_loop_start = actionable_indices
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

        // Find newly ready effects: all deps completed and not yet dispatched
        let mut newly_ready: Vec<usize> = Vec::new();
        if !cancelled {
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
        }

        // Emit Waiting events for effects that have unmet dependencies
        if !cancelled {
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
        }

        let available = input.parallelism.get().saturating_sub(in_flight.len());
        newly_ready.truncate(available);

        // Process newly ready effects: skip those with failed deps, spawn the rest
        let mut completed_synchronous_dispatch = false;
        for idx in newly_ready {
            dispatched.insert(idx);
            let effect = effects[idx].clone();

            let failure_view = FailureView::new(&effects, &deps_of, &failed_indices);
            if let Some(failed_dep) = failure_view.find_failed_dependency(idx) {
                let c = if effect.is_scheduler_meta() {
                    completed.load(Ordering::Relaxed)
                } else {
                    completed.fetch_add(1, Ordering::Relaxed) + 1
                };
                let reason = if effect.is_wait() {
                    wait_dependency_failed_reason(&failed_dep)
                } else {
                    dependency_failed_reason(&failed_dep)
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
                failed_indices.insert(idx);
                completed_indices.insert(idx);
                continue;
            }

            let dispatch_ctx = PureMetaCtx {
                completed: &completed,
                total,
                observer,
            };
            match try_dispatch_pure_meta(&effect, &input.bindings, &dispatch_ctx) {
                PureMetaOutcome::NotPureMeta => {}
                PureMetaOutcome::Failed => {
                    failure_count += 1;
                    failed_indices.insert(idx);
                    completed_indices.insert(idx);
                    completed_synchronous_dispatch = true;
                    break;
                }
                PureMetaOutcome::Materialized(children) => {
                    if !children.is_empty() {
                        total += count_actionable_effects(&children);
                        for child in children {
                            let child_idx = effects.len();
                            if let Effect::Create(resource) = &child {
                                runtime_synthesized_resources.push(resource.clone().into_inner());
                            }
                            if let Some(binding) = failure_binding_name(&child) {
                                idx_to_binding.insert(child_idx, binding);
                            }
                            effects.push(child);
                            actionable_indices.push(child_idx);
                        }
                        deps_of = build_scheduler_deps(
                            &effects,
                            input.unresolved_resources,
                            input.compositions,
                            &deferred_replace_delete_deps,
                        );
                    }
                    completed_indices.insert(idx);
                    completed_synchronous_dispatch = true;
                    break;
                }
            }

            // Snapshot bindings for this effect's resolution.
            let binding_snapshot = input.bindings.clone();
            let wait_identifiers = wait_identifiers.clone();
            let unresolved = &input.unresolved_resources;
            let pipeline = RenormalizePipeline {
                normalizer: input.normalizer,
                provider_configs: input.provider_configs,
                factories: input.factories,
                schemas: input.schemas,
            };
            let completed_ref = &completed;
            let effect_for_future = effect.clone();
            let make_future = move |wait_cancel_rx: Option<
                tokio::sync::watch::Receiver<WaitSignal>,
            >| async move {
                let result = match effect_for_future.as_basic() {
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
                    None => match &effect_for_future {
                        Effect::Create(_) | Effect::Update { .. } | Effect::Delete { .. } => {
                            // `as_basic()` returns `Some` for exactly these
                            // three variants; they're handled by the `Some`
                            // arm above.
                            unreachable!("Create/Update/Delete are narrowed by as_basic()")
                        }
                        Effect::Read { .. } => SingleEffectResult::ReadNoOp,
                        // State operations are handled separately during apply
                        Effect::Import { .. } | Effect::Remove { .. } | Effect::Move { .. } => {
                            SingleEffectResult::ReadNoOp
                        }
                        Effect::DeferredCreate { .. } => unreachable!(
                            "DeferredCreate is handled synchronously before provider dispatch"
                        ),
                        Effect::DeferredReplace { .. } => unreachable!(
                            "DeferredReplace is handled synchronously before provider dispatch"
                        ),
                        Effect::Wait {
                            identity,
                            target_id,
                            until,
                            timeout,
                            interval,
                            ..
                        } => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            observer.on_event(&ExecutionEvent::EffectStarted {
                                effect: &effect_for_future,
                            });
                            let progress = ProgressInfo {
                                completed: c,
                                total,
                            };
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
                                wait_cancel_rx.expect("wait dispatch must have a cancel receiver"),
                                observer,
                            )
                            .await;
                            SingleEffectResult::Wait {
                                binding: identity.to_string(),
                                outcome,
                                duration: started.elapsed(),
                                progress,
                            }
                        }
                    },
                };
                (idx, result)
            };

            if effect.is_wait() {
                in_flight.push_wait(idx, |cancel_rx| Box::pin(make_future(Some(cancel_rx))));
            } else {
                in_flight.push_non_wait(idx, make_future(None));
            }
        }

        if completed_synchronous_dispatch {
            continue;
        }

        let count_undispatched = |dispatched: &HashSet<usize>, failed_indices: &HashSet<usize>| {
            let failure_view = FailureView::new(&effects, &deps_of, failed_indices);
            count_effectively_undispatched(&actionable_indices, dispatched, &failure_view)
        };
        in_flight
            .check_terminal(count_undispatched(&dispatched, &failed_indices))
            .cancel_if_terminal()
            .drop_without_awaiting();

        // If nothing is in flight, we're done (or stuck in a cycle)
        if in_flight.is_empty() {
            // Check for undispatched effects (would indicate a dependency cycle)
            let remaining = actionable_indices
                .iter()
                .filter(|idx| !dispatched.contains(idx))
                .count();
            if cancelled {
                let mut progress_for = |_| ProgressInfo {
                    completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
                    total,
                };
                emit_cancelled_skips_with_progress(
                    &effects,
                    &actionable_indices,
                    &mut dispatched,
                    &mut completed_indices,
                    &mut skip_count,
                    observer,
                    &mut progress_for,
                );
            } else if remaining > 0 {
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
        let (finished_idx, result) = if cancelled {
            let Some(finished) = in_flight
                .check_terminal(count_undispatched(&dispatched, &failed_indices))
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
                    .check_terminal(count_undispatched(&dispatched, &failed_indices))
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

        // Process the result and update shared state immediately
        match result {
            SingleEffectResult::Basic(basic) => {
                process_basic_result(
                    basic,
                    &mut ExecutionState {
                        idx: finished_idx,
                        success_count: &mut success_count,
                        failure_count: &mut failure_count,
                        partial_count: &mut partial_count,
                        partial_diagnostics: &mut partial_diagnostics,
                        applied_states: &mut applied_states,
                        failed_indices: &mut failed_indices,
                        successfully_deleted: &mut successfully_deleted,
                        pending_refreshes: &mut pending_refreshes,
                        bindings: &mut input.bindings,
                    },
                );
            }
            SingleEffectResult::ReadNoOp => {}
            SingleEffectResult::Wait {
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
                    // Register the captured target snapshot under the
                    // wait's *synthetic* ResourceId so the downstream
                    // resolution layer can deref `<binding>.<attr>` —
                    // and under the binding name in `bindings` so
                    // resolve_refs sees the same attribute map. Wait
                    // effects do not persist to the state file
                    // (handled by `state_writeback_should_skip`).
                    let synthetic = ResourceId::with_identity("__wait", &binding);
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
                WaitOutcome::Unsatisfiable(reason) => {
                    let detail = unsatisfiable_reason_message(&reason);
                    let reason = format!("unsatisfiable: {detail}");
                    observer.on_event(&ExecutionEvent::EffectSkipped {
                        effect: &effects[finished_idx],
                        reason: &reason,
                        progress,
                    });
                    skip_count += 1;
                    failed_indices.insert(finished_idx);
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
                    let error = wait_failure_message(&outcome, effects[finished_idx].resource_id());
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect: &effects[finished_idx],
                        error: &error,
                        duration,
                        progress,
                    });
                    failure_count += 1;
                    failed_indices.insert(finished_idx);
                }
            },
        }
        in_flight
            .check_terminal(count_undispatched(&dispatched, &failed_indices))
            .cancel_if_terminal()
            .drop_without_awaiting();
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
        partial_count,
        partial_diagnostics,
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
    use crate::effect::deps::reads::{KnownReads, ReadsSet};
    use crate::effect::deps::{
        ScheduleInputs, WritesSet, build_effect_dependency_analysis as build_dependency_analysis,
        relax_update_update_edges,
    };
    use crate::plan::Plan;
    use crate::provider::{
        BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, ProviderError, ProviderResult,
        ReadRequest, UpdateRequest,
    };
    use crate::resource::{
        Composition, ConcreteValue, DataSource, ResolvedResource, ResourceIdentity, State, Value,
    };
    use crate::schema::SchemaRegistry;
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn state_for(id: &ResourceId) -> State {
        State::existing(id.clone(), HashMap::new()).with_identifier("id")
    }

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn update_effect(binding: &str, reads: &[(&str, &str)], writes: &[&str]) -> Effect {
        let id = ResourceId::with_identity("test", binding);
        let mut to = Resource::new("test", binding);
        to.binding = Some(binding.to_string());
        for (dep, attr) in reads {
            to.set_attr(
                format!("{}_{}", dep, attr),
                Value::resource_ref(*dep, *attr, vec![]),
            );
        }
        Effect::Update {
            from: Box::new(state_for(&id)),
            to: resolved(to),
            changed_attributes: writes.iter().map(|attr| (*attr).to_string()).collect(),
        }
    }

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
        ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
            self.create_log
                .lock()
                .unwrap()
                .push(id.identity_or_empty().to_string());
            let id = id.clone();
            Box::pin(async move {
                if id.identity_or_empty() == "alb" {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Err(ProviderError::api_error("alb create failed"))
                } else {
                    Ok(crate::provider::CreateOutcome::Success {
                        state: State::existing(id, HashMap::new()).with_identifier("cert-id"),
                    })
                }
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
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
                | ExecutionEvent::EffectPartiallySucceeded { .. }
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

    fn dependency_after_relax(parent: Effect, child: Effect) -> HashSet<usize> {
        let effects = vec![parent, child];
        let unresolved: HashMap<ResourceId, UnresolvedResource> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Create(resource) | Effect::Update { to: resource, .. } => Some((
                    resource.id.clone(),
                    UnresolvedResource::from_pre_resolve(resource.as_inner().clone()),
                )),
                Effect::DeferredReplace { .. } => None,
                _ => None,
            })
            .collect();
        let mut analysis =
            build_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply);
        relax_update_update_edges(&effects, &mut analysis);
        analysis.into_deps_of().remove(&1).unwrap()
    }

    #[tokio::test]
    async fn wait_marked_unsatisfiable_when_only_waits_in_flight() {
        let provider = TerminalWaitProvider::new();

        let mut cert = Resource::new("test", "cert");
        cert.binding = Some("cert".to_string());
        let cert_id = cert.id.clone();

        let mut alb = Resource::new("test", "alb");
        alb.binding = Some("alb".to_string());

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(cert.clone())));
        plan.add(Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: crate::resource::ResolvedResourceId::new(cert_id.clone()),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_millis(1),
            explicit_dependencies: std::collections::HashSet::new(),
        });
        plan.add(Effect::Create(resolved(alb.clone())));

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
            execute_effects_sequential(&provider, &mut input, &observer, &CancellationToken::new()),
        )
        .await
        .expect("wait should be skipped as unsatisfiable instead of timing out");
        let (result, was_cancelled) = result;
        assert!(!was_cancelled);

        assert_eq!(result.failure_count, 1, "alb create should fail");
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
    async fn wait_marked_unsatisfiable_when_failing_sibling_blocks_consumer_inside_wait_subtree() {
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
        plan.add(Effect::Create(resolved(cert.clone())));
        plan.add(Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: crate::resource::ResolvedResourceId::new(cert_id.clone()),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_millis(1),
            explicit_dependencies: std::collections::HashSet::new(),
        });
        plan.add(Effect::Create(resolved(alb.clone())));
        plan.add(Effect::Create(resolved(listener.clone())));

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
        let deps =
            build_dependency_analysis(plan.effects(), &unresolved, &[], ScheduleInputs::Apply)
                .into_deps_of();
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
            execute_effects_sequential(&provider, &mut input, &observer, &CancellationToken::new()),
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
    fn reads_set_merge_and_disjoint_keep_unknown_distinct_from_empty_known() {
        let known_id = ReadsSet::from_walker(KnownReads::from_attrs(&["id"]));
        let known_tags = ReadsSet::from_walker(KnownReads::from_attrs(&["tags"]));
        let merged = known_id.merge(known_tags);
        assert!(matches!(
            merged,
            ReadsSet::Known(attrs) if attrs.attrs() == &set(&["id", "tags"])
        ));

        let unknown =
            ReadsSet::from_walker(KnownReads::from_attrs(&["id"])).merge(ReadsSet::unknown());
        assert!(unknown.is_unknown());

        let update = update_effect("parent", &[], &["tags"]);
        let writes = WritesSet::from_update(&update).unwrap();
        assert!(ReadsSet::from_walker(KnownReads::from_attrs(&[])).disjoint(&writes));
        assert!(ReadsSet::from_walker(KnownReads::from_attrs(&["id"])).disjoint(&writes));
        assert!(!ReadsSet::from_walker(KnownReads::from_attrs(&["tags"])).disjoint(&writes));
        assert!(!ReadsSet::unknown().disjoint(&writes));
        assert!(
            WritesSet::from_update(&Effect::Create(resolved(Resource::new("test", "x")))).is_none()
        );
    }

    #[test]
    fn relax_update_update_edge_when_child_reads_disjoint_attribute() {
        let deps = dependency_after_relax(
            update_effect("parent", &[], &["tags"]),
            update_effect("child", &[("parent", "id")], &["tags"]),
        );
        assert!(!deps.contains(&0), "disjoint update edge should be relaxed");
    }

    #[test]
    fn keep_update_update_edge_when_child_reads_written_attribute() {
        let deps = dependency_after_relax(
            update_effect("parent", &[], &["tags"]),
            update_effect("child", &[("parent", "tags")], &["name"]),
        );
        assert!(deps.contains(&0), "overlapping read/write edge must remain");
    }

    #[test]
    fn keep_update_update_edge_for_bare_binding_unknown_read() {
        let parent = update_effect("parent", &[], &["cidr_block"]);
        let mut child = match update_effect("child", &[], &["name"]) {
            Effect::Update { to, .. } => to.into_inner(),
            _ => unreachable!(),
        };
        child.set_attr(
            "whole_parent",
            Value::Deferred(crate::resource::DeferredValue::BindingRef {
                binding: "parent".to_string(),
            }),
        );
        let child = Effect::Update {
            from: Box::new(state_for(&child.id)),
            to: resolved(child),
            changed_attributes: vec!["name".to_string()],
        };

        let deps = dependency_after_relax(parent, child);
        assert!(deps.contains(&0), "unknown bare-binding read must remain");
    }

    #[test]
    fn keep_update_update_edge_when_known_read_also_has_depends_on_unknown() {
        let parent = update_effect("parent", &[], &["tags"]);
        let mut child = match update_effect("child", &[("parent", "id")], &["name"]) {
            Effect::Update { to, .. } => to.into_inner(),
            _ => unreachable!(),
        };
        child.directives.depends_on.push("parent".to_string());
        let child = Effect::Update {
            from: Box::new(state_for(&child.id)),
            to: resolved(child),
            changed_attributes: vec!["name".to_string()],
        };

        let deps = dependency_after_relax(parent, child);
        assert!(
            deps.contains(&0),
            "depends_on must promote reads to unknown"
        );
    }

    #[test]
    fn dependency_bindings_only_path_escalates_to_unknown() {
        let parent = update_effect("parent", &[], &["tags"]);
        let mut child = match update_effect("child", &[], &["name"]) {
            Effect::Update { to, .. } => to.into_inner(),
            _ => unreachable!(),
        };
        child.dependency_bindings.insert("parent".to_string());
        let child_id = child.id.clone();
        let effects = vec![
            parent,
            Effect::Update {
                from: Box::new(state_for(&child_id)),
                to: resolved(child.clone()),
                changed_attributes: vec!["name".to_string()],
            },
        ];
        let unresolved = HashMap::from([
            (
                effects[0].resource_id().clone(),
                UnresolvedResource::from_pre_resolve(match &effects[0] {
                    Effect::Update { to, .. } => to.as_inner().clone(),
                    _ => unreachable!(),
                }),
            ),
            (child_id, UnresolvedResource::from_pre_resolve(child)),
        ]);

        let analysis = build_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply);
        assert!(
            analysis
                .reads_for_edge(1, 0)
                .is_some_and(ReadsSet::is_unknown),
            "dependency_bindings-only edge must be unknown"
        );

        let mut analysis = analysis;
        relax_update_update_edges(&effects, &mut analysis);
        assert!(
            analysis.into_deps_of()[&1].contains(&0),
            "unknown dependency_bindings edge must not be relaxed",
        );
    }

    #[test]
    fn create_parent_update_child_is_not_relaxed() {
        let mut parent = Resource::new("test", "parent");
        parent.binding = Some("parent".to_string());
        let deps = dependency_after_relax(
            Effect::Create(resolved(parent)),
            update_effect("child", &[("parent", "id")], &["tags"]),
        );
        assert!(
            deps.contains(&0),
            "Create parent is outside relaxation scope"
        );
    }

    #[test]
    fn keep_update_update_edge_for_composition_expansion_unknown_read() {
        let parent = update_effect("parent", &[], &["tags"]);
        let mut child = match update_effect("child", &[], &["name"]) {
            Effect::Update { to, .. } => to.into_inner(),
            _ => unreachable!(),
        };
        child.set_attr(
            "forwarded",
            Value::resource_ref("module", "parent_id", vec![]),
        );

        let mut virt_attrs: indexmap::IndexMap<String, crate::resource::CompositionAttribute> =
            indexmap::IndexMap::new();
        virt_attrs.insert(
            "parent_id".to_string(),
            crate::resource::CompositionAttribute::from_value(Value::resource_ref(
                "parent",
                "id",
                vec![],
            )),
        );
        let virt = Composition {
            id: ResourceId::with_provider_identity("_virtual", "_virtual", "module", None),
            signature: crate::resource::Signature {
                arguments: indexmap::IndexMap::new(),
                attributes: virt_attrs,
            },
            binding: Some("module".to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "network".to_string(),
            instance: "module".to_string(),
            quoted_string_attrs: std::collections::HashSet::new(),
        };

        let child_id = child.id.clone();
        let effects = vec![
            parent,
            Effect::Update {
                from: Box::new(state_for(&child_id)),
                to: resolved(child.clone()),
                changed_attributes: vec!["name".to_string()],
            },
        ];
        let unresolved = HashMap::from([
            (
                effects[0].resource_id().clone(),
                UnresolvedResource::from_pre_resolve(match &effects[0] {
                    Effect::Update { to, .. } => to.as_inner().clone(),
                    _ => unreachable!(),
                }),
            ),
            (child_id, UnresolvedResource::from_pre_resolve(child)),
        ]);
        let mut analysis =
            build_dependency_analysis(&effects, &unresolved, &[virt], ScheduleInputs::Apply);
        relax_update_update_edges(&effects, &mut analysis);
        let deps_of = analysis.into_deps_of();

        assert!(
            deps_of[&1].contains(&0),
            "composition expansion must promote the edge to unknown and keep it",
        );
    }

    #[test]
    fn relax_update_update_edges_handles_empty_plan() {
        let effects = Vec::new();
        let mut analysis =
            build_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply);
        relax_update_update_edges(&effects, &mut analysis);
        assert!(analysis.into_deps_of().is_empty());
    }

    #[test]
    fn relax_update_update_edges_handles_single_update_without_parent() {
        let effect = update_effect("only", &[], &["tags"]);
        let effects = vec![effect];
        let mut analysis =
            build_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply);
        relax_update_update_edges(&effects, &mut analysis);
        assert!(analysis.into_deps_of()[&0].is_empty());
    }

    #[test]
    fn relax_update_update_edges_ignores_parent_update_without_update_children() {
        let child = Resource::new("test", "child");
        let effects = vec![
            update_effect("parent", &[], &["tags"]),
            Effect::Create(resolved(child)),
        ];
        let mut analysis =
            build_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply);
        relax_update_update_edges(&effects, &mut analysis);
        assert!(analysis.into_deps_of()[&0].is_empty());
    }

    /// Mirror of #2543's phased-executor test for the unphased dependency map:
    /// composition module-attribute proxies must be transparently followed to the
    /// underlying resources their attributes reference.
    #[test]
    fn build_dependency_analysis_follows_virtual_module_binding() {
        let mut role = Resource::with_provider("awscc", "iam.Role", "bootstrap.role", None);
        role.binding = Some("bootstrap.role".to_string());

        // carina#3181: composition resources are a distinct typestate.
        let mut virt_attrs: indexmap::IndexMap<String, crate::resource::CompositionAttribute> =
            indexmap::IndexMap::new();
        virt_attrs.insert(
            "role_name".to_string(),
            crate::resource::CompositionAttribute::from_value(Value::resource_ref(
                "bootstrap.role",
                "role_name",
                vec![],
            )),
        );
        let virt = Composition {
            id: ResourceId::with_provider_identity("_virtual", "_virtual", "bootstrap", None),
            signature: crate::resource::Signature {
                arguments: indexmap::IndexMap::new(),
                attributes: virt_attrs,
            },
            binding: Some("bootstrap".to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "github-oidc".to_string(),
            instance: "bootstrap".to_string(),
            quoted_string_attrs: std::collections::HashSet::new(),
        };

        let mut role_policy = Resource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        role_policy.set_attr(
            "role_name",
            Value::resource_ref("bootstrap", "role_name", vec![]),
        );

        let effects = vec![
            Effect::Create(resolved(role.clone())),
            Effect::Create(resolved(role_policy.clone())),
        ];

        let mut unresolved: HashMap<ResourceId, UnresolvedResource> = HashMap::new();
        unresolved.insert(
            role.id.clone(),
            UnresolvedResource::from_pre_resolve(role.clone()),
        );
        unresolved.insert(
            role_policy.id.clone(),
            UnresolvedResource::from_pre_resolve(role_policy),
        );

        let deps_of =
            build_dependency_analysis(&effects, &unresolved, &[virt], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps_of[&1].contains(&0),
            "RolePolicy (idx 1) must depend on Role (idx 0) via the bootstrap composition binding; got: {:?}",
            deps_of[&1],
        );
    }
}
