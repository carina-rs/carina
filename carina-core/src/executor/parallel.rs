//! Dependency computation and parallel effect execution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::effect::Effect;
#[cfg(test)]
use crate::effect::deps::UnresolvedResource;
use crate::provider::{AppliedIdentifier, CancelledCreateReport, Provider, ProviderRouter};
#[cfg(test)]
use crate::resource::Resource;
use crate::resource::{ResourceId, ResourceIdentity};
use crate::shutdown::{CleanupInterrupted, LoopShutdownPhase, LoopStep, ShutdownToken};

use super::basic::{
    BasicEffectCompletion, BasicEffectCtx, ExecutionState, count_actionable_effects,
    execute_basic_effect, process_basic_result, refresh_pending_states, resolve_data_source,
};
use super::deferred_dispatch::{
    DeferredAnonymousIdentityGuard, PureMetaCtx, report_deferred_create_failure,
};
use super::provider_ready::ProviderPreparationContext;
use super::replace::SingleEffectResult;
use super::scheduler::{
    FailureView, PureMetaOutcome, SchedulerDeps, dependency_failed_reason, failure_display_name,
    try_dispatch_pure_meta, wait_dependency_failed_reason,
};
use super::wait::{
    SKIP_REASON_CANCELLED, WaitAwareInFlight, WaitEffectRef, WaitOutcome, WaitSignal,
    count_effectively_undispatched, resolve_wait_identifier, unsatisfiable_reason_message,
    wait_failure_message,
};
use super::{
    Applied, ExecutionAccumulator, ExecutionEvent, ExecutionObserver, ExecutionResult,
    ProgressInfo, RuntimeExecutionInput, unresolved_data_source_inputs,
};
#[cfg(test)]
use super::{ExecutionInput, ProviderCheckInputs};

pub(super) struct ExpandedEffects<S> {
    pub(super) effects: Vec<Effect<S>>,
    pub(super) deferred_replace_delete_deps: Vec<(usize, usize)>,
    pub(super) source_effect_indices: Vec<usize>,
}

pub(super) fn expand_deferred_replace_effects<S: Clone>(
    plan_effects: &[Effect<S>],
) -> ExpandedEffects<S> {
    let mut effects = Vec::with_capacity(plan_effects.len());
    let mut deferred_replace_delete_deps = Vec::new();
    let mut source_effect_indices = Vec::with_capacity(plan_effects.len());

    for (source_effect_idx, effect) in plan_effects.iter().enumerate() {
        if let Effect::DeferredReplace(payload) = effect {
            let mut delete_indices = Vec::with_capacity(payload.deletes.len());
            for delete in payload.deletes.iter() {
                let delete_idx = effects.len();
                effects.push(delete.to_delete_effect());
                source_effect_indices.push(source_effect_idx);
                delete_indices.push(delete_idx);
            }
            let gate_idx = effects.len();
            effects.push(effect.clone());
            source_effect_indices.push(source_effect_idx);
            deferred_replace_delete_deps.extend(
                delete_indices
                    .into_iter()
                    .map(|delete_idx| (gate_idx, delete_idx)),
            );
        } else {
            effects.push(effect.clone());
            source_effect_indices.push(source_effect_idx);
        }
    }

    ExpandedEffects {
        effects,
        deferred_replace_delete_deps,
        source_effect_indices,
    }
}

pub(super) fn apply_deferred_replace_delete_deps<S>(
    effects: &[Effect<S>],
    deps_of: &mut HashMap<usize, HashSet<usize>>,
    deferred_replace_delete_deps: &[(usize, usize)],
) {
    // An unrelated anonymous state row remains a normal Delete rather than
    // being claimed by one arbitrary unbound loop. Every same-kind unbound
    // runtime gate must nevertheless wait for that row to be removed, because
    // any gate may materialize a child with the persisted generated address.
    for (delete_idx, effect) in effects.iter().enumerate() {
        let Effect::Delete {
            id,
            generation: crate::effect::EffectGeneration::Current,
            binding: None,
            ..
        } = effect
        else {
            continue;
        };
        if !deps_of.contains_key(&delete_idx) {
            continue;
        }
        for (gate_idx, candidate) in effects.iter().enumerate() {
            let template = match candidate {
                Effect::DeferredCreate { template, .. } => template.as_ref(),
                Effect::DeferredReplace(payload) => payload.template.as_ref(),
                _ => continue,
            };
            if matches!(
                template.target,
                crate::parser::DeferredForTarget::Unbound { .. }
            ) && template.template_resource.id.provider == id.provider
                && template.template_resource.id.resource_type == id.resource_type
                && template.template_resource.id.provider_instance == id.provider_instance
            {
                deps_of.entry(gate_idx).or_default().insert(delete_idx);
            }
        }
    }

    for &(gate_idx, delete_idx) in deferred_replace_delete_deps {
        let Some(Effect::DeferredReplace(payload)) = effects.get(gate_idx) else {
            continue;
        };

        if deps_of.contains_key(&delete_idx) {
            // The gate owns its absorbed rows. A failed or skipped gate must
            // therefore skip the deletes instead of destroying live state
            // before the replacement shape is known.
            deps_of.entry(delete_idx).or_default().insert(gate_idx);
        }

        let is_unbound = matches!(
            payload.template.target,
            crate::parser::DeferredForTarget::Unbound { .. }
        );

        let Some(delete_id) = effects.get(delete_idx).map(Effect::resource_id) else {
            continue;
        };
        for (create_idx, effect) in effects.iter().enumerate() {
            let Effect::Create(resource) = effect else {
                continue;
            };
            let same_live_id = resource.id == *delete_id;
            let replaces_legacy_loop_row = is_unbound
                && resource.id.provider == delete_id.provider
                && resource.id.resource_type == delete_id.resource_type
                && resource.id.provider_instance == delete_id.provider_instance
                && resource.id.identity().is_some_and(|identity| {
                    resource
                        .loop_provenance
                        .legacy_candidates(identity)
                        .iter()
                        .any(|legacy| delete_id.identity_str() == Some(legacy.exact_state_name()))
                });
            if !same_live_id && !replaces_legacy_loop_row {
                continue;
            }
            // DeferredReplace materializes the replacement create after the
            // delete placeholder is already in the scheduler. The generic
            // same-identity rule would otherwise order create after delete.
            if same_live_id && let Some(create_deps) = deps_of.get_mut(&create_idx) {
                create_deps.remove(&delete_idx);
            }
            deps_of.entry(delete_idx).or_default().insert(create_idx);
        }
    }
}

pub(super) fn is_runtime_dispatchable<S: Clone>(
    effect: &Effect<S>,
    data_source_values: &crate::data_source_value::ExecutableDataSourceValues<S>,
) -> bool {
    is_deferred_read(effect, data_source_values)
        || (!matches!(effect, Effect::Read { .. })
            && (!effect.is_state_operation() || effect.is_scheduler_meta()))
}

pub(super) fn is_runtime_noop<S: Clone>(
    effect: &Effect<S>,
    data_source_values: &crate::data_source_value::ExecutableDataSourceValues<S>,
) -> bool {
    match effect {
        Effect::Read { .. } => !is_deferred_read(effect, data_source_values),
        Effect::Import { .. }
        | Effect::Remove { .. }
        | Effect::Move { .. }
        | Effect::ReleaseListKeys { .. } => true,
        Effect::Create(_)
        | Effect::ResumeCreate { .. }
        | Effect::Update { .. }
        | Effect::Delete { .. }
        | Effect::Wait { .. }
        | Effect::DeferredCreate { .. }
        | Effect::DeferredReplace(_) => false,
    }
}

fn count_runtime_effects<S: Clone>(
    effects: &[Effect<S>],
    data_source_values: &crate::data_source_value::ExecutableDataSourceValues<S>,
) -> usize {
    count_actionable_effects(effects)
        + effects
            .iter()
            .filter(|effect| is_deferred_read(effect, data_source_values))
            .count()
}

fn is_deferred_read<S: Clone>(
    effect: &Effect<S>,
    data_source_values: &crate::data_source_value::ExecutableDataSourceValues<S>,
) -> bool {
    matches!(effect, Effect::Read { resource } if data_source_values.deferred_inputs(&resource.id).is_some())
}

fn format_unresolved_read_inputs(unresolved: &[super::UnresolvedDataSourceInput]) -> String {
    unresolved
        .iter()
        .map(|input| {
            let mut refs = input
                .paths
                .iter()
                .map(|path| path.to_dot_string())
                .collect::<Vec<_>>();
            refs.extend(input.bindings.iter().cloned());
            refs.extend(
                input
                    .unknowns
                    .iter()
                    .map(|reason| format!("unknown({reason})")),
            );
            let refs = refs.join(", ");
            format!("{} -> {}", input.attribute, refs)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
pub(super) fn build_dependency_levels(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::LiveComposition],
) -> Vec<Vec<usize>> {
    let deps_of = crate::effect::deps::build_effect_dependency_analysis(
        effects,
        unresolved_resources,
        compositions,
        crate::effect::deps::ScheduleInputs::Apply,
    )
    .expect("dependency analysis must resolve")
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

pub(super) async fn execute_effects_sequential<
    S: Clone + crate::resource::AnonymousIdentityModuleSource,
>(
    provider: &dyn Provider,
    comparison_provider: &ProviderRouter,
    input: &mut RuntimeExecutionInput<'_, S>,
    observer: &dyn ExecutionObserver<S>,
    shutdown: &ShutdownToken,
) -> (ExecutionResult<S>, bool) {
    let result = ExecutionResult {
        success_count: input.precompleted_success_count,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        interrupted_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        successfully_deleted: HashSet::new(),
        permanent_name_overrides: HashMap::new(),
        managed_current: std::mem::take(&mut input.managed_current),
        data_source_states: input.data_source_values.read_states_map(),
        bindings: std::mem::take(&mut input.bindings),
        failed_refreshes: HashSet::new(),
    };
    let (mut accumulated, wait_identifiers) = ExecutionAccumulator::new(result);
    if input.precompleted_success_count > 0 {
        accumulated.mark_dirty();
    }
    let mut pending_refreshes: HashMap<ResourceId, AppliedIdentifier> = HashMap::new();
    let mut runtime_authored_resource_ids: HashSet<ResourceId> = HashSet::new();
    let module_gate = input.compositions.module_constraint_gate();

    let ExpandedEffects {
        effects: expanded_effects,
        deferred_replace_delete_deps,
        source_effect_indices: _,
    } = expand_deferred_replace_effects(input.plan.effects());
    let mut effects = expanded_effects;
    let mut total = count_runtime_effects(&effects, &input.data_source_values);
    let completed = AtomicUsize::new(0);

    let empty_resource_origins = HashMap::new();
    let resource_origins = input
        .provider_check_inputs
        .resource_origins()
        .unwrap_or(&empty_resource_origins);
    let mut desired_ids = resource_origins.keys().cloned().collect::<HashSet<_>>();
    for effect in &effects {
        if matches!(
            effect,
            Effect::Read { .. }
                | Effect::Create(_)
                | Effect::ResumeCreate { .. }
                | Effect::Update { .. }
                | Effect::Import { .. }
                | Effect::Move { .. }
                | Effect::Wait { .. }
        ) {
            desired_ids.insert(effect.resource_id().clone());
        }
    }
    let current_ids = accumulated
        .result()
        .managed_current
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let absorbed_current_ids = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::DeferredReplace(payload)
                if matches!(
                    payload.template.target,
                    crate::parser::DeferredForTarget::Unbound { .. }
                ) =>
            {
                Some(payload.deletes.iter())
            }
            _ => None,
        })
        .flatten()
        .map(|delete| delete.id.as_inner().clone())
        .collect();
    let mut anonymous_identity_guard =
        DeferredAnonymousIdentityGuard::new(desired_ids, current_ids, absorbed_current_ids);
    let SchedulerDeps {
        mut deps_of,
        unresolved: scheduler_dependency_errors,
    } = input.compositions.scheduler_dependencies(
        &effects,
        resource_origins,
        &deferred_replace_delete_deps,
    );

    // Build effect index -> binding name mapping for resolving dependency names
    let mut idx_to_binding: HashMap<usize, String> = effects
        .iter()
        .enumerate()
        .filter_map(|(idx, effect)| failure_display_name(effect).map(|b| (idx, b)))
        .collect();

    // Track which effect indices have completed (successfully or not)
    let mut completed_indices: HashSet<usize> = HashSet::new();
    // Track effect indices whose failure should poison direct scheduler dependents.
    let mut failed_indices: HashSet<usize> = HashSet::new();
    // Track which effect indices have been dispatched (spawned or skipped)
    let mut dispatched: HashSet<usize> = HashSet::new();

    for dependency in scheduler_dependency_errors {
        let idx = dependency.effect_index();
        let message = dependency.to_string();
        let progress = ProgressInfo {
            completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
            total,
        };
        observer.on_event(&ExecutionEvent::EffectFailed {
            effect: &effects[idx],
            error: &message,
            recovery: None,
            duration: std::time::Duration::ZERO,
            progress,
        });
        accumulated.mutate(|result| result.failure_count += 1);
        completed_indices.insert(idx);
        failed_indices.insert(idx);
        dispatched.insert(idx);
    }
    accumulated.publish_if_dirty(&input.checkpoint_sink);
    // All actionable effect indices (excluding Read and state operations)
    let mut actionable_indices: Vec<usize> = (0..effects.len())
        .filter(|&idx| is_runtime_dispatchable(&effects[idx], &input.data_source_values))
        .collect();

    // Mark pre-apply Read and plain state operation effects as completed
    // (they are no-ops in the executor).
    // DeferredCreate is state-only for progress/provider purposes, but it is a scheduler
    // dispatch point that materializes dynamic Create effects.
    for (idx, effect) in effects.iter().enumerate() {
        if is_runtime_noop(effect, &input.data_source_values) {
            completed_indices.insert(idx);
            dispatched.insert(idx);
        }
    }

    let mut in_flight: WaitAwareInFlight<'_, SingleEffectResult<'_, S>, S> =
        WaitAwareInFlight::new();
    let mut cancelled = false;
    let cleanup_loop = shutdown.cleanup_aware_loop();

    'execution: loop {
        let undispatched_at_loop_start = actionable_indices
            .iter()
            .filter(|&&idx| !dispatched.contains(&idx))
            .count();
        let cleanup_wait = match cleanup_loop.step() {
            LoopStep::Continue(wait) => wait,
            LoopStep::Abandon => {
                cancelled = true;
                break 'execution;
            }
        };
        match cleanup_wait.phase() {
            LoopShutdownPhase::Graceful
                if !cancelled && (undispatched_at_loop_start > 0 || !in_flight.is_empty()) =>
            {
                cancelled = true;
                in_flight.signal_in_flight_waits();
            }
            LoopShutdownPhase::Running | LoopShutdownPhase::Graceful => {}
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
                accumulated.mutate(|result| result.skip_count += 1);
                failed_indices.insert(idx);
                completed_indices.insert(idx);
                completed_synchronous_dispatch = true;
                continue;
            }

            let dispatch_ctx = PureMetaCtx {
                completed: &completed,
                total,
                observer,
                identity_inputs: super::deferred_dispatch::DeferredIdentityInputs {
                    provider_configs: input.provider_configs,
                    factories: input.factories,
                    schemas: input.schemas,
                },
            };
            match try_dispatch_pure_meta(&effect, &accumulated.result().bindings, &dispatch_ctx) {
                PureMetaOutcome::NotPureMeta => {}
                PureMetaOutcome::Failed => {
                    accumulated.mutate(|result| result.failure_count += 1);
                    failed_indices.insert(idx);
                    completed_indices.insert(idx);
                    completed_synchronous_dispatch = true;
                    break;
                }
                PureMetaOutcome::Materialized(children) => {
                    let template = match &effect {
                        Effect::DeferredCreate { template, .. } => template.as_ref(),
                        Effect::DeferredReplace(payload) => payload.template.as_ref(),
                        _ => unreachable!("pure meta materialization has a deferred template"),
                    };
                    if let Err(error) = anonymous_identity_guard.admit(template, &children) {
                        report_deferred_create_failure(&effect, &error, &dispatch_ctx);
                        accumulated.mutate(|result| result.failure_count += 1);
                        failed_indices.insert(idx);
                        completed_indices.insert(idx);
                        completed_synchronous_dispatch = true;
                        break;
                    }
                    if !children.is_empty() {
                        total += count_actionable_effects(&children);
                        for child in children {
                            let child_idx = effects.len();
                            if let Effect::Create(resource) = &child {
                                runtime_authored_resource_ids.insert(resource.id.clone());
                                accumulated.mutate(|result| {
                                    result.runtime_synthesized_resources.push(resource.clone());
                                });
                            }
                            if let Some(binding) = failure_display_name(&child) {
                                idx_to_binding.insert(child_idx, binding);
                            }
                            effects.push(child);
                            actionable_indices.push(child_idx);
                        }
                        let rebuilt = input.compositions.scheduler_dependencies(
                            &effects,
                            resource_origins,
                            &deferred_replace_delete_deps,
                        );
                        deps_of = rebuilt.deps_of;
                        for dependency in rebuilt.unresolved {
                            let failed_idx = dependency.effect_index();
                            if !dispatched.insert(failed_idx) {
                                continue;
                            }
                            let message = dependency.to_string();
                            let progress = ProgressInfo {
                                completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
                                total,
                            };
                            observer.on_event(&ExecutionEvent::EffectFailed {
                                effect: &effects[failed_idx],
                                error: &message,
                                recovery: None,
                                duration: std::time::Duration::ZERO,
                                progress,
                            });
                            accumulated.mutate(|result| result.failure_count += 1);
                            completed_indices.insert(failed_idx);
                            failed_indices.insert(failed_idx);
                        }
                    }
                    completed_indices.insert(idx);
                    completed_synchronous_dispatch = true;
                    break;
                }
            }

            // Snapshot bindings for this effect's resolution.
            let binding_snapshot = accumulated.result().bindings.clone();
            let wait_identifiers = wait_identifiers.clone();
            let provider_check_inputs = &input.provider_check_inputs;
            let runtime_authored = runtime_authored_resource_ids.contains(effect.resource_id());
            let module_gate = &module_gate;
            let normalizer = input.normalizer;
            let provider_configs = input.provider_configs;
            let factories = input.factories;
            let schemas = input.schemas;
            let resume_create = match &effect {
                Effect::ResumeCreate { resource, .. } => accumulated
                    .result()
                    .managed_current
                    .get(&resource.id)
                    .and_then(|current| match current {
                        crate::resource::Current::CreateInFlight(pending) => Some(pending.clone()),
                        crate::resource::Current::Absent(_)
                        | crate::resource::Current::Present(_)
                        | crate::resource::Current::CreateOutcomeUnknown(_)
                        | crate::resource::Current::MissingIdentifier(_) => None,
                    }),
                Effect::Read { .. }
                | Effect::Create(_)
                | Effect::Update { .. }
                | Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::ReleaseListKeys { .. }
                | Effect::Wait { .. }
                | Effect::DeferredCreate { .. }
                | Effect::DeferredReplace(_) => None,
            };
            let completed_ref = &completed;
            let effect_for_future = effect.clone();
            let make_future = move |wait_cancel_rx: Option<
                tokio::sync::watch::Receiver<WaitSignal>,
            >,
                                    in_flight_start| async move {
                let preparation = ProviderPreparationContext::new(
                    &binding_snapshot,
                    module_gate,
                    provider_configs,
                    normalizer,
                    factories,
                    schemas,
                );
                let result = match effect_for_future.as_basic() {
                    // `BasicEffect` is the type-level contract for
                    // `execute_basic_effect`: any Create/Update/Delete
                    // narrows here, and any non-basic variant falls
                    // through to the `None` arm so it can't accidentally
                    // be dispatched (carina#3164). The compiler enforces
                    // exhaustiveness on the outer `match effect { ... }`
                    // below.
                    Some(basic) => SingleEffectResult::Basic(Box::new(
                        execute_basic_effect(
                            basic,
                            &BasicEffectCtx {
                                provider,
                                comparison_provider,
                                provider_check_inputs,
                                runtime_authored,
                                preparation: &preparation,
                                resume_create,
                                completed: completed_ref,
                                total,
                            },
                            in_flight_start,
                            observer,
                        )
                        .await,
                    )),
                    None => match &effect_for_future {
                        Effect::Create(_)
                        | Effect::ResumeCreate { .. }
                        | Effect::Update { .. }
                        | Effect::Delete { .. } => {
                            // `as_basic()` returns `Some` for exactly these
                            // three variants; they're handled by the `Some`
                            // arm above.
                            unreachable!("Create/Update/Delete are narrowed by as_basic()")
                        }
                        Effect::Read { resource } => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            let progress = ProgressInfo {
                                completed: c,
                                total,
                            };
                            let started_effect = in_flight_start.emit_started(
                                observer,
                                &effect_for_future,
                                progress,
                            );
                            let mut resolved = resource.as_inner().clone();
                            let outcome = {
                                let check_input =
                                    provider_check_inputs.data_source_input(resource.as_inner());
                                match resolve_data_source(check_input, &binding_snapshot) {
                                    Ok(resolved_input) => {
                                        resolved = resolved_input.value().clone();
                                        let resolved_secret_shapes =
                                            crate::resource::attrs_to_hashmap(
                                                &resolved_input.value().attributes,
                                            );
                                        let unresolved =
                                            unresolved_data_source_inputs(resolved_input.value());
                                        if unresolved.is_empty() {
                                            match super::provider_ready::prepare_provider_ready_data_source_after_resolution(
                                                resolved_input,
                                                &preparation,
                                            ) {
                                                Ok(ready) => {
                                                    let provider_values =
                                                        crate::resource::attrs_to_hashmap(
                                                            &ready.as_data_source().attributes,
                                                        );
                                                    let resolved_attrs = crate::binding_index::SecretPreservingAttributes::from_provider_values(
                                                        &resolved_secret_shapes,
                                                        &provider_values,
                                                    );
                                                    super::read_data_source_with_retry(
                                                        provider, &ready,
                                                    )
                                                    .await
                                                    .map(|state| (resolved_attrs, state))
                                                    .map_err(|err| {
                                                        format!(
                                                            "data source read failed for {}: {err}",
                                                            resolved.id
                                                        )
                                                    })
                                                }
                                                Err(err) => Err(format!(
                                                    "data source preparation failed for {}: {err}",
                                                    resolved.id
                                                )),
                                            }
                                        } else {
                                            Err(format!(
                                                "data source inputs for {} still contain \
                                                 unresolved references after dependencies \
                                                 completed: {}",
                                                resolved.id,
                                                format_unresolved_read_inputs(&unresolved)
                                            ))
                                        }
                                    }
                                    Err(error) => Err(format!(
                                        "failed to resolve data source inputs for {}: {error}",
                                        resolved.id
                                    )),
                                }
                            };
                            SingleEffectResult::Read {
                                started: started_effect,
                                resource: Box::new(resolved),
                                outcome,
                                duration: started.elapsed(),
                            }
                        }
                        // State operations are handled separately during apply
                        Effect::Import { .. }
                        | Effect::Remove { .. }
                        | Effect::Move { .. }
                        | Effect::ReleaseListKeys { .. } => SingleEffectResult::ReadNoOp,
                        Effect::DeferredCreate { .. } => unreachable!(
                            "DeferredCreate is handled synchronously before provider dispatch"
                        ),
                        Effect::DeferredReplace(_) => unreachable!(
                            "DeferredReplace is handled synchronously before provider dispatch"
                        ),
                        Effect::Wait { .. } => {
                            let c = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            let started = Instant::now();
                            let progress = ProgressInfo {
                                completed: c,
                                total,
                            };
                            let started_effect = in_flight_start.emit_started(
                                observer,
                                &effect_for_future,
                                progress,
                            );
                            let wait = WaitEffectRef::from_effect(&effect_for_future)
                                .expect("Effect::Wait must produce a wait view");
                            let identifier_resolver = |target_id: &ResourceId| {
                                resolve_wait_identifier(&wait_identifiers, target_id)
                            };
                            let cleanup_loop = shutdown.cleanup_aware_loop();
                            let target_binding =
                                match binding_snapshot.source(wait.identity().as_str()) {
                                    Some(crate::binding_index::BindingValueSource::WaitAlias {
                                        target,
                                    }) => target.as_str().to_string(),
                                    Some(
                                        crate::binding_index::BindingValueSource::Local
                                        | crate::binding_index::BindingValueSource::Upstream,
                                    )
                                    | None => wait.target().id().identity_str().to_string(),
                                };
                            let binding = wait.identity().to_string();
                            let outcome = super::wait::execute_wait_effect(
                                &cleanup_loop,
                                provider,
                                wait,
                                &identifier_resolver,
                                wait_cancel_rx.expect("wait dispatch must have a cancel receiver"),
                                observer,
                            )
                            .await;
                            SingleEffectResult::Wait {
                                started: started_effect,
                                binding,
                                target_binding,
                                outcome,
                                duration: started.elapsed(),
                            }
                        }
                    },
                };
                (idx, result)
            };

            if effect.is_wait() {
                in_flight.push_wait(idx, |cancel_rx, start| {
                    Box::pin(make_future(Some(cancel_rx), start))
                });
            } else {
                in_flight.push_non_wait(idx, |start| make_future(None, start));
            }
        }

        if completed_synchronous_dispatch {
            accumulated.publish_if_dirty(&input.checkpoint_sink);
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
                let progress_for = |_| ProgressInfo {
                    completed: completed.fetch_add(1, Ordering::Relaxed) + 1,
                    total,
                };
                for &idx in &actionable_indices {
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
                    accumulated.mutate(|result| result.skip_count += 1);
                }
            } else if remaining > 0 {
                // Cycle detected: skip remaining effects as failures
                for &idx in &actionable_indices {
                    if !dispatched.contains(&idx) {
                        dispatched.insert(idx);
                        completed_indices.insert(idx);
                        accumulated.mutate(|result| result.failure_count += 1);
                    }
                }
            }
            break;
        }

        // Wait for the next effect to complete. The completion branch stays
        // ahead of shutdown so a result that is already ready is harvested.
        let completion = {
            let next_completed = in_flight
                .check_terminal(count_undispatched(&dispatched, &failed_indices))
                .cancel_if_terminal()
                .next_completed();
            let completion = cleanup_wait.until_cleanup_priority(next_completed);
            tokio::pin!(completion);
            if cancelled {
                Some(completion.await)
            } else {
                tokio::select! {
                    biased;
                    completion = &mut completion => Some(completion),
                    _ = shutdown.cancelled() => None,
                }
            }
        };
        let Some(completion) = completion else {
            cancelled = true;
            in_flight.signal_in_flight_waits();
            continue;
        };
        let (finished_idx, effect_result) = match completion {
            CleanupInterrupted::Completed(Some(finished)) => finished,
            CleanupInterrupted::Completed(None) => break 'execution,
            CleanupInterrupted::Abandoned => {
                cancelled = true;
                break 'execution;
            }
        };
        completed_indices.insert(finished_idx);

        // Process the result and update shared state immediately
        match effect_result {
            SingleEffectResult::Basic(basic) => {
                let completion = process_basic_result(
                    *basic,
                    &mut ExecutionState {
                        idx: finished_idx,
                        accumulator: &mut accumulated,
                        failed_indices: &mut failed_indices,
                        pending_refreshes: &mut pending_refreshes,
                    },
                );
                if completion == BasicEffectCompletion::Deleted
                    && let Some(Effect::Delete {
                        id,
                        generation: crate::effect::EffectGeneration::Current,
                        binding: None,
                        ..
                    }) = effects.get(finished_idx)
                {
                    anonymous_identity_guard.release_completed_current_delete(id.as_inner());
                }
            }
            SingleEffectResult::ReadNoOp => {}
            SingleEffectResult::Read {
                started,
                resource,
                outcome,
                duration,
            } => match outcome {
                Ok((resolved_attrs, state)) => {
                    started.succeeded(Some(&state), duration);
                    accumulated.mutate(|result| {
                        result.success_count += 1;
                        result.bindings.record_applied(
                            resource.binding.as_deref(),
                            &resolved_attrs,
                            &state,
                            crate::binding_index::BindingCompleteness::Complete,
                        );
                        result
                            .data_source_states
                            .insert(resource.id.clone(), state.clone());
                    });
                    accumulated.insert_published(
                        resource.id.clone(),
                        crate::executor::Applied::DataSource(state.clone()),
                        &state,
                    );
                }
                Err(error) => {
                    started.failed(&error, duration);
                    accumulated.mutate(|result| result.failure_count += 1);
                    failed_indices.insert(finished_idx);
                }
            },
            SingleEffectResult::Wait {
                started,
                binding,
                target_binding,
                outcome,
                duration,
            } => match outcome {
                WaitOutcome::Satisfied { state } => {
                    // Register the captured target snapshot under the
                    // wait's *synthetic* ResourceId so the downstream
                    // resolution layer can deref `<binding>.<attr>` —
                    // and under the binding name in `bindings` so
                    // resolve_refs sees the same attribute map. Wait
                    // effects do not persist to the state file
                    // (handled by `state_writeback_should_skip`).
                    let synthetic =
                        ResourceId::with_identity("__wait", ResourceIdentity::new(binding.clone()));
                    match accumulated
                        .result()
                        .bindings
                        .secret_preserving_wait_snapshot(&target_binding, &state)
                    {
                        Ok(attrs) => {
                            started.succeeded(Some(&state), duration);
                            accumulated.mutate(|result| {
                                result.success_count += 1;
                                result.bindings.record_applied(
                                    Some(&binding),
                                    &attrs,
                                    &state,
                                    crate::binding_index::BindingCompleteness::Complete,
                                );
                            });
                            accumulated.insert_published(
                                synthetic,
                                crate::executor::Applied::WaitSnapshot(state.clone()),
                                &state,
                            );
                        }
                        Err(error) => {
                            started.failed(&error, duration);
                            accumulated.mutate(|result| result.failure_count += 1);
                            failed_indices.insert(finished_idx);
                        }
                    }
                }
                WaitOutcome::Unsatisfiable(reason) => {
                    let detail = unsatisfiable_reason_message(&reason);
                    let reason = format!("unsatisfiable: {detail}");
                    started.skipped(&reason);
                    accumulated.mutate(|result| result.skip_count += 1);
                    failed_indices.insert(finished_idx);
                }
                WaitOutcome::Cancelled | WaitOutcome::Abandoned => {
                    started.skipped(SKIP_REASON_CANCELLED);
                    accumulated.mutate(|result| result.skip_count += 1);
                }
                outcome @ (WaitOutcome::Timeout { .. }
                | WaitOutcome::NotFound(_)
                | WaitOutcome::ReadFailed(_)) => {
                    let error = wait_failure_message(&outcome, effects[finished_idx].resource_id());
                    started.failed(&error, duration);
                    accumulated.mutate(|result| result.failure_count += 1);
                    failed_indices.insert(finished_idx);
                }
            },
        }
        accumulated.publish_if_dirty(&input.checkpoint_sink);
        in_flight
            .check_terminal(count_undispatched(&dispatched, &failed_indices))
            .cancel_if_terminal()
            .drop_without_awaiting();
    }

    // Cleanup-priority mode deliberately drops unresolved provider futures so
    // the caller can persist the results already folded into `applied_states`.
    // Every effect that reached EffectStarted receives a terminal event before
    // state persistence; queued but never-polled futures are omitted.
    // Every loop exit passes through this point, so the total includes both
    // cleanup abandonment and any release-mode fail-safe interruptions that
    // were emitted while processing a completed future.
    let abandonment = in_flight.abandon(observer);
    if abandonment.interrupted_count > 0 {
        accumulated.mutate(|result| result.interrupted_count += abandonment.interrupted_count);
    }
    for report in abandonment.create_reports {
        match report {
            CancelledCreateReport::Untracked => {}
            CancelledCreateReport::OutcomeUnknown(unknown) => {
                accumulated.insert_unpublished(
                    unknown.id().clone(),
                    Applied::CreateOutcomeUnknown(unknown),
                );
            }
        }
    }
    accumulated.publish_if_dirty(&input.checkpoint_sink);
    let refresh_cancelled = match cleanup_loop.step() {
        LoopStep::Abandon => true,
        LoopStep::Continue(cleanup_wait) => {
            let refresh = refresh_pending_states(
                provider,
                &mut accumulated,
                &pending_refreshes,
                observer,
                &input.checkpoint_sink,
            );
            match cleanup_wait.until_cleanup_priority(refresh).await {
                CleanupInterrupted::Completed(()) => false,
                CleanupInterrupted::Abandoned => true,
            }
        }
    };
    cancelled |= refresh_cancelled;

    let report_unresolved_constraints =
        accumulated.result().failure_count == 0 && accumulated.result().skip_count == 0;
    if !cancelled
        && let Err(error) = module_gate.finish(
            &accumulated.result().bindings,
            report_unresolved_constraints,
        )
    {
        for failure in error.failures() {
            let message = failure.to_string();
            observer.on_event(&ExecutionEvent::ModuleConstraintFailed { error: &message });
            accumulated.mutate(|result| result.failure_count += 1);
        }
    }
    let result = accumulated.finish(&input.checkpoint_sink);
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
        AppliedState, BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, ProviderError,
        ProviderResult, ReadRequest, UpdateRequest,
    };
    use crate::resource::{
        ConcreteValue, Directives, LiveComposition, ResolvedResource, ResourceIdentity, State,
        Value,
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

    fn unbound_deferred_template_in(file: &str) -> crate::parser::DeferredForExpression {
        let parsed = crate::parser::parse_file(
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.accounts {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
            std::path::Path::new(file),
            &crate::parser::ProviderContext::default(),
        )
        .expect("unbound deferred fixture parses");
        let parsed =
            crate::module_resolver::resolve_modules_providerless(parsed, std::path::Path::new("."))
                .expect("deferred fixture crosses the module-resolution seam");
        let parsed = parsed
            .into_expanded()
            .expect("deferred fixture crosses the placeholder boundary");
        parsed
            .into_deferred_for_expressions()
            .into_iter()
            .next()
            .expect("fixture contains one deferred loop")
    }

    #[test]
    fn all_same_kind_unbound_deferred_gates_wait_for_anonymous_delete() {
        let deleted_id = crate::resource::ResolvedResourceId::with_provider_identity(
            "mock",
            "test.Child",
            "mock_test_child_deadbeef",
            None,
        );
        let effects = vec![
            Effect::Delete {
                id: deleted_id,
                identifier: crate::effect::fixture_applied_identifier("old-child"),
                generation: crate::effect::EffectGeneration::Current,
                directives: Directives::default(),
                binding: None,
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
            Effect::DeferredCreate {
                id: crate::resource::ResolvedResourceId::with_provider_identity(
                    "mock",
                    "__deferred_for",
                    "first_loop",
                    None,
                ),
                template: Box::new(unbound_deferred_template_in("a.crn")),
            },
            Effect::DeferredCreate {
                id: crate::resource::ResolvedResourceId::with_provider_identity(
                    "mock",
                    "__deferred_for",
                    "second_loop",
                    None,
                ),
                template: Box::new(unbound_deferred_template_in("b.crn")),
            },
        ];
        let mut deps_of = HashMap::from([
            (0usize, HashSet::new()),
            (1usize, HashSet::new()),
            (2usize, HashSet::new()),
        ]);

        apply_deferred_replace_delete_deps(&effects, &mut deps_of, &[]);

        assert_eq!(deps_of.get(&1), Some(&HashSet::from([0])));
        assert_eq!(deps_of.get(&2), Some(&HashSet::from([0])));
    }

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn managed_wait_target(id: ResourceId) -> crate::wait::ManagedWaitTarget {
        crate::wait::ManagedWaitTarget::new(&ResolvedResource::from_id(
            crate::resource::ResolvedResourceId::new(id),
        ))
    }

    fn update_effect(binding: &'static str, reads: &[(&str, &str)], writes: &[&str]) -> Effect {
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
            identifier: crate::effect::fixture_applied_identifier("id"),
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
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ReadOutcome>> {
            let mut attrs = HashMap::new();
            attrs.insert(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
            );
            let state = crate::provider::AppliedState::new(id.clone(), identifier.as_str(), attrs)
                .expect("typed read identifier is non-empty");
            Box::pin(async move { Ok(crate::provider::ReadOutcome::found(state)) })
        }

        fn read_data_source(
            &self,
            _resource: &crate::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
            attempt: crate::provider::CreateAttempt,
        ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>>
        {
            self.create_log
                .lock()
                .unwrap()
                .push(id.identity_str().expect("resolved identity").to_string());
            let id = id.clone();
            Box::pin(async move {
                if id.identity_str().expect("resolved identity") == "alb" {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Err(attempt.untracked(ProviderError::api_error("alb create failed")))
                } else {
                    Ok(crate::provider::CreateOutcome::fixture_success(
                        AppliedState::new(id, "cert-id", HashMap::new())
                            .expect("fixture identifier is non-empty"),
                    ))
                }
            })
        }

        fn resume_create(
            &self,
            _request: crate::provider::ResumeCreateRequest,
        ) -> BoxFuture<'_, Result<crate::provider::ResumeOutcome, crate::provider::ResumeFailure>>
        {
            Box::pin(async { panic!("unexpected resume_create") })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("update not used")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("delete not used")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
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
                | ExecutionEvent::EffectStarted {
                    effect: _,
                    start: _,
                }
                | ExecutionEvent::EffectInterrupted {
                    effect: _,
                    progress: _,
                }
                | ExecutionEvent::EffectSucceeded { .. }
                | ExecutionEvent::EffectPartiallySucceeded { .. }
                | ExecutionEvent::WaitPolling {
                    effect: _,
                    observation: _,
                    elapsed: _,
                }
                | ExecutionEvent::CascadeUpdateSucceeded { .. }
                | ExecutionEvent::CascadeUpdateFailed { .. }
                | ExecutionEvent::RenameSucceeded { .. }
                | ExecutionEvent::RenameFailed { .. }
                | ExecutionEvent::RefreshStarted
                | ExecutionEvent::RefreshSucceeded { .. }
                | ExecutionEvent::RefreshFailed { .. }
                | ExecutionEvent::ModuleConstraintFailed { .. } => {}
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
                Effect::DeferredReplace(_) => None,
                _ => None,
            })
            .collect();
        let mut analysis =
            build_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply)
                .expect("dependency analysis must resolve");
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
            target_id: managed_wait_target(cert_id.clone()),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
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
        let (schemas, comparison_provider) = crate::provider::comparison_fixture_for_test(
            &SchemaRegistry::new(),
            plan.effects().iter().map(Effect::resource_id),
        );
        let mut input: RuntimeExecutionInput = ExecutionInput {
            plan: &plan,
            provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                resource_origins: &unresolved,
                data_source_origins: &[],
            },
            compositions: &[],
            bindings: Default::default(),
            managed_current: HashMap::new(),
            data_source_values: crate::data_source_value::ExecutableDataSourceValues::empty(),
            normalizer: &NoopNormalizer,
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
            parallelism: std::num::NonZeroUsize::new(2).unwrap(),
            precompleted_success_count: 0,
            checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
        }
        .into();
        let observer = RecordingSkipObserver::new();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            execute_effects_sequential(
                &provider,
                &comparison_provider,
                &mut input,
                &observer,
                &ShutdownToken::running(),
            ),
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
            target_id: managed_wait_target(cert_id.clone()),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
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
                .expect("dependency analysis must resolve")
                .into_deps_of();
        assert!(
            deps.get(&3).is_some_and(|listener_deps| {
                listener_deps.contains(&1) && listener_deps.contains(&2)
            }),
            "listener should depend on both alb and cert_issued; deps: {deps:?}"
        );

        let (schemas, comparison_provider) = crate::provider::comparison_fixture_for_test(
            &SchemaRegistry::new(),
            plan.effects().iter().map(Effect::resource_id),
        );
        let mut input: RuntimeExecutionInput = ExecutionInput {
            plan: &plan,
            provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                resource_origins: &unresolved,
                data_source_origins: &[],
            },
            compositions: &[],
            bindings: Default::default(),
            managed_current: HashMap::new(),
            data_source_values: crate::data_source_value::ExecutableDataSourceValues::empty(),
            normalizer: &NoopNormalizer,
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
            parallelism: std::num::NonZeroUsize::new(2).unwrap(),
            precompleted_success_count: 0,
            checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
        }
        .into();
        let observer = RecordingSkipObserver::new();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            execute_effects_sequential(
                &provider,
                &comparison_provider,
                &mut input,
                &observer,
                &ShutdownToken::running(),
            ),
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
            identifier: crate::effect::fixture_applied_identifier("id"),
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
            identifier: crate::effect::fixture_applied_identifier("id"),
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
                identifier: crate::effect::fixture_applied_identifier("id"),
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

        let analysis = build_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply)
            .expect("dependency analysis must resolve");
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

        let mut virt_attrs: indexmap::IndexMap<String, crate::resource::LiveCompositionAttribute> =
            indexmap::IndexMap::new();
        virt_attrs.insert(
            "parent_id".to_string(),
            crate::resource::LiveCompositionAttribute::from_value(
                Value::resource_ref("parent", "id", vec![]),
                None,
            ),
        );
        let virt = LiveComposition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
                "_virtual", "_virtual", "module", None,
            )),
            signature: crate::resource::LiveCompositionSignature {
                arguments: indexmap::IndexMap::new(),
                attributes: virt_attrs,
                pending_constraints: Vec::new(),
            },
            binding: Some("module".to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "network".to_string(),
            instance: "module".to_string(),
            call_path: Default::default(),
            quoted_string_attrs: std::collections::HashSet::new(),
        };

        let child_id = child.id.clone();
        let effects = vec![
            parent,
            Effect::Update {
                identifier: crate::effect::fixture_applied_identifier("id"),
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
            build_dependency_analysis(&effects, &unresolved, &[virt], ScheduleInputs::Apply)
                .expect("dependency analysis must resolve");
        relax_update_update_edges(&effects, &mut analysis);
        let deps_of = analysis.into_deps_of();

        assert!(
            deps_of[&1].contains(&0),
            "composition expansion must promote the edge to unknown and keep it",
        );
    }

    #[test]
    fn relax_update_update_edges_handles_empty_plan() {
        let effects: Vec<Effect> = Vec::new();
        let mut analysis =
            build_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .expect("dependency analysis must resolve");
        relax_update_update_edges(&effects, &mut analysis);
        assert!(analysis.into_deps_of().is_empty());
    }

    #[test]
    fn relax_update_update_edges_handles_single_update_without_parent() {
        let effect = update_effect("only", &[], &["tags"]);
        let effects = vec![effect];
        let mut analysis =
            build_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .expect("dependency analysis must resolve");
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
            build_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .expect("dependency analysis must resolve");
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
        let mut virt_attrs: indexmap::IndexMap<String, crate::resource::LiveCompositionAttribute> =
            indexmap::IndexMap::new();
        virt_attrs.insert(
            "role_name".to_string(),
            crate::resource::LiveCompositionAttribute::from_value(
                Value::resource_ref("bootstrap.role", "role_name", vec![]),
                None,
            ),
        );
        let virt = LiveComposition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
                "_virtual",
                "_virtual",
                "bootstrap",
                None,
            )),
            signature: crate::resource::LiveCompositionSignature {
                arguments: indexmap::IndexMap::new(),
                attributes: virt_attrs,
                pending_constraints: Vec::new(),
            },
            binding: Some("bootstrap".to_string()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "github-oidc".to_string(),
            instance: "bootstrap".to_string(),
            call_path: Default::default(),
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
                .expect("dependency analysis must resolve")
                .into_deps_of();

        assert!(
            deps_of[&1].contains(&0),
            "RolePolicy (idx 1) must depend on Role (idx 0) via the bootstrap composition binding; got: {:?}",
            deps_of[&1],
        );
    }
}
