use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use colored::Colorize;
use futures::stream::{FuturesUnordered, StreamExt};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::get_resource_dependencies;
use carina_core::effect::deps::{
    DependencyAnalysis, DestroyWaitAlias, ScheduleInputs, UnresolvedResource,
    build_effect_dependency_analysis,
};
use carina_core::effect::{Effect, EffectGeneration};
use carina_core::parser::WaitBinding;
use carina_core::plan::{Plan, PlanError};
use carina_core::provider::Provider;
#[cfg(test)]
use carina_core::resource::State;
use carina_core::resource::{
    ConcreteValue, Current, ResolvedResourceId, Resource, ResourceId, Value,
};
#[cfg(test)]
use carina_core::shutdown::testing::TestShutdownTrigger;
use carina_core::shutdown::{
    CleanupInterrupted, LoopShutdownPhase, LoopStep, ShutdownPhase, ShutdownToken,
};
#[cfg(test)]
use carina_state::LockInfo;
use carina_state::{StateBackend, StateFile};

use carina_core::parser::ProviderContext;

use super::{DriftCommand, verify_for_mutation};
use crate::DetailLevel;
use crate::commands::plan::collect_historical_display_attributes;
use crate::commands::plan::{
    ProviderUpstreamScope, UpstreamMissingStatePolicy, load_and_resolve_provider_configs,
};
use crate::commands::shared::finalize::{
    LockCommandOutcome, StatePersistence, finalize_after_execute, finish_lock_command,
};
use crate::commands::shared::incremental_state_saver::{FinalStateBase, IncrementalStateSaver};
use crate::commands::shared::lock_heartbeat::{
    HeartbeatOutcome, StateLock, StateLockRef, with_lock_heartbeat,
};
use crate::commands::shared::plan_errors::render_plan_errors_and_abort;
use crate::commands::shared::progress::{
    FailedOutcome, NonFailureOutcome, ProgressBackend, ProgressGuard, ProgressSink, RefreshPhase,
    format_duration,
};
use crate::commands::shared::retry::is_retryable_delete_error;
use crate::commands::shared::state_writeback::{
    DestroyStateWrite, DestroyedInstance, apply_destroy_to_state, build_orphan_resource,
};
use crate::commands::state::map_lock_error;
use crate::cursor::CursorReveal;
use crate::display::{format_destroy_plan_with_delete_instances, format_effect};
use crate::error::{AppError, DestroyDependencyAnalysisError};
use crate::wiring::{
    ProviderLoad, ValidatedConfig, get_provider_with_ctx, refresh_loaded_orphan_with_retry,
    refresh_loaded_resource_with_retry,
};

struct DestroyInFlight<'a, F> {
    idx: usize,
    progress: Option<ProgressGuard<'a, usize>>,
    future: Pin<Box<F>>,
}

impl<'a, F> DestroyInFlight<'a, F> {
    fn new(idx: usize, progress: ProgressGuard<'a, usize>, future: F) -> Self {
        Self {
            idx,
            progress: Some(progress),
            future: Box::pin(future),
        }
    }

    fn into_progress(mut self) -> (usize, ProgressGuard<'a, usize>) {
        (
            self.idx,
            self.progress
                .take()
                .expect("in-flight destroy effect must own progress"),
        )
    }
}

impl<'a, F: Future> Future for DestroyInFlight<'a, F> {
    type Output = (usize, ProgressGuard<'a, usize>, F::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => Poll::Ready((
                this.idx,
                this.progress
                    .take()
                    .expect("completed destroy effect must own progress"),
                output,
            )),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_destroy(
    user_dirs: &carina_core::UserDirs,
    path: &Path,
    auto_approve: bool,
    lock: bool,
    refresh: bool,
    force: bool,
    parallelism: NonZeroUsize,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    let providers = ProviderLoad::load(user_dirs, &parsed.providers, base_dir)?;
    let validated_config = providers.validate_evaluated_config(
        parsed,
        base_dir,
        true,
        &inference_errors,
        &duplicate_declarations,
    )?;

    let verified_backend = verify_for_mutation(
        path,
        validated_config.resources().backend.as_ref(),
        DriftCommand::Destroy,
    )?;

    // Don't exit early when resources are empty -- orphaned resources in the
    // state file may still need to be destroyed.

    // Check for backend configuration - use local backend by default
    let backend: Arc<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?
        .into();

    // Get the state bucket name for protection check (S3 backend only)
    let protected_bucket = verified_backend
        .s3_settings()
        .map(|settings| settings.bucket().to_owned());

    // Acquire lock (unless --lock=false)
    let state_lock = if lock {
        println!("{}", "Acquiring state lock...".cyan());
        let li = backend
            .acquire_lock("destroy")
            .await
            .map_err(|error| map_lock_error(error, path))?;
        println!("  {} Lock acquired", "✓".green());
        StateLock::held(li)
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow()
                .bold()
        );
        StateLock::disabled_by_cli_flag()
    };

    let outcome = run_destroy_locked_owned(
        validated_config,
        auto_approve,
        Arc::clone(&backend),
        protected_bucket,
        state_lock,
        refresh,
        force,
        base_dir,
        provider_context,
        parallelism,
        cancel.clone(),
    )
    .await;
    finish_lock_command(backend.as_ref(), outcome, &cancel).await?;

    Ok(())
}

enum DestroyAfterHeartbeat {
    Done,
    Executed(Box<DestroyExecuted>),
}

struct DestroyExecuted {
    state_base: FinalStateBase,
    destroyed_instances: Vec<DestroyedInstance>,
    success_count: usize,
    failure_count: usize,
    skip_count: usize,
    unknown_count: usize,
    cancelled: bool,
    cancel: ShutdownToken,
}

struct DestroyedInstances {
    instances: Vec<DestroyedInstance>,
    publisher: tokio::sync::watch::Sender<Option<Arc<Vec<DestroyedInstance>>>>,
}

impl DestroyedInstances {
    fn new(publisher: tokio::sync::watch::Sender<Option<Arc<Vec<DestroyedInstance>>>>) -> Self {
        Self {
            instances: Vec::new(),
            publisher,
        }
    }

    fn record(&mut self, destroyed: DestroyedInstance) {
        self.instances.push(destroyed);
        // ponytail: this clones the cumulative destroyed-instance list once
        // per completion; use delta checkpoints if very large plans make that
        // remaining ceiling material.
        self.publisher
            .send_replace(Some(Arc::new(self.instances.clone())));
    }

    fn into_instances(self) -> Vec<DestroyedInstance> {
        self.instances
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_destroy_locked_owned(
    mut validated_config: ValidatedConfig<carina_core::parser::InferredFile>,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    protected_bucket: Option<String>,
    lock: StateLock,
    refresh: bool,
    force: bool,
    base_dir: &std::path::Path,
    provider_context: &ProviderContext,
    parallelism: NonZeroUsize,
    cancel: ShutdownToken,
) -> LockCommandOutcome<Result<(), AppError>> {
    // Read current state from backend. carina#3315: persist any older-schema
    // migration under the destroy lock before any short-circuit
    // (e.g. "No resources to destroy.") returns — see
    // `apply::load_state_persist_if_migrated`.
    let mut state_file = match crate::commands::apply::load_state_persist_if_migrated(
        backend.as_ref(),
        lock.as_ref(),
    )
    .await
    {
        Ok(state) => state,
        Err(error) => {
            return LockCommandOutcome::Kept {
                result: Err(error),
                lock,
            };
        }
    };
    if let Err(error) = crate::commands::apply::prune_legacy_data_source_rows_persist_if_changed(
        backend.as_ref(),
        lock.as_ref(),
        &mut state_file,
        &validated_config.resources().data_sources,
    )
    .await
    {
        return LockCommandOutcome::Kept {
            result: Err(error),
            lock,
        };
    }
    let heartbeat = with_lock_heartbeat(
        Arc::clone(&backend),
        lock,
        cancel,
        async |cancel, mid_run_writer| {
    let state_block_claims = validated_config.reconcile_for_destroy(&state_file);
    let (ctx, parsed) = validated_config.context_and_resources();

    let provider_inputs = load_and_resolve_provider_configs(
        parsed,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
        ProviderUpstreamScope::ProviderReferences,
    )
    .await?;

    // Collect all resources (managed + orphans) before sorting.
    // We use the unsorted list for state reads, then sort once at the end.
    let mut all_resources = crate::wiring::DeferredForExpansionPrelude::new_for_state_refresh(
        parsed,
        &state_file,
        ctx.schemas(),
    )?
    .detect_moves(
        ctx,
        &parsed.data_sources,
        provider_inputs.configs().as_provider_configs(),
        &state_block_claims,
    )?
    .into_destroy_resources();

    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached managed-resource state (--refresh=false). The plan may not reflect actual managed infrastructure.".yellow()
        );
    }

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(
        ctx,
        provider_inputs.configs(),
        ProgressSink::Human,
    )
    .await?;

    // Build managed current state -- either from the provider or the state file.
    let mut managed_current: HashMap<ResourceId, Current> = HashMap::new();

    if refresh {
        let progress_backend = ProgressBackend::new(ProgressSink::Human);
        let refresh_phase = RefreshPhase::start(&progress_backend);

        // Read states for managed resources concurrently using identifier
        // from state. carina#3181: `all_resources` is sourced from
        // `parsed.resources`, which is managed-only — data sources and
        // composition resources are not destroyed and never enter this list.
        let resources: Vec<&Resource> = all_resources.iter().collect();
        let provider_ref = &provider;
        let persisted_state = state_file.as_ref();
        let schemas = ctx.schemas();
        let mut refresh_cancelled = false;
        let mut resource_iter = resources.into_iter();
        let mut refresh_in_flight = FuturesUnordered::new();
        let cleanup_loop = cancel.cleanup_aware_loop();
        loop {
            let cleanup_wait = match cleanup_loop.step() {
                LoopStep::Continue(wait) => wait,
                LoopStep::Abandon => {
                    refresh_cancelled = true;
                    break;
                }
            };
            match cleanup_wait.phase() {
                LoopShutdownPhase::Running => {}
                LoopShutdownPhase::Graceful => refresh_cancelled = true,
            }

            while !refresh_cancelled && refresh_in_flight.len() < 5 {
                let Some(resource) = resource_iter.next() else {
                    break;
                };
                let progress = refresh_phase.begin(&resource.id);
                let loaded = persisted_state
                    .map(|sf| sf.build_state_for_resource(&resource.id))
                    .unwrap_or_else(|| Current::absent(resource.id.clone()));
                refresh_in_flight.push(async move {
                    let current = refresh_loaded_resource_with_retry(
                        provider_ref,
                        loaded,
                        persisted_state,
                        schemas,
                    )
                    .await
                    .map_err(AppError::Provider)?;
                    progress.finish();
                    Ok((resource.id.clone(), current))
                });
            }

            if refresh_in_flight.is_empty() {
                break;
            }

            let draining = refresh_cancelled;
            #[cfg(test)]
            let next_in_flight = async {
                if draining {
                    crate::commands::shared::cancellation_test_support::observe_refresh_drain_wait(
                        &cancel,
                    )
                    .await;
                }
                refresh_in_flight.next().await
            };
            #[cfg(not(test))]
            let next_in_flight = refresh_in_flight.next();
            // A ready provider result is harvested before cleanup priority. If
            // refresh is already cancelled, the short-circuit below discards it.
            let next = cleanup_wait.until_cleanup_priority(next_in_flight);
            tokio::pin!(next);
            let next = if draining {
                next.await
            } else {
                tokio::select! {
                    biased;
                    next = &mut next => next,
                    _ = cancel.cancelled() => {
                        refresh_cancelled = true;
                        continue;
                    }
                }
            };
            let result: Result<(ResourceId, Current), AppError> = match next {
                CleanupInterrupted::Completed(Some(result)) => result,
                CleanupInterrupted::Completed(None) => break,
                CleanupInterrupted::Abandoned => {
                    refresh_cancelled = true;
                    break;
                }
            };

            if refresh_cancelled {
                continue;
            }
            let (id, current) = result?;
            managed_current.insert(id, current);
        }
        drop(refresh_in_flight);
        drop(resource_iter);
        if refresh_cancelled {
            return Err(AppError::Interrupted);
        }

        // Include orphaned resources (in state but not in .crn).
        // Refresh each orphan concurrently via provider.read() to verify it still exists.
        if let Some(sf) = persisted_state {
            let desired_ids: HashSet<ResourceId> =
                all_resources.iter().map(|r| r.id.clone()).collect();
            let loaded_orphans = sf.build_orphans(&desired_ids);
            let orphan_entries: Vec<_> = loaded_orphans.into_iter().collect();
            let mut refresh_cancelled = false;
            let mut orphan_iter = orphan_entries.into_iter();
            let mut orphan_in_flight = FuturesUnordered::new();
            let cleanup_loop = cancel.cleanup_aware_loop();
            loop {
                let cleanup_wait = match cleanup_loop.step() {
                    LoopStep::Continue(wait) => wait,
                    LoopStep::Abandon => {
                        refresh_cancelled = true;
                        break;
                    }
                };
                match cleanup_wait.phase() {
                    LoopShutdownPhase::Running => {}
                    LoopShutdownPhase::Graceful => refresh_cancelled = true,
                }

                while !refresh_cancelled && orphan_in_flight.len() < 5 {
                    let Some((id, orphan)) = orphan_iter.next() else {
                        break;
                    };
                    let progress = refresh_phase.begin(&id);
                    orphan_in_flight.push(async move {
                        let refreshed = refresh_loaded_orphan_with_retry(
                            provider_ref,
                            orphan,
                            persisted_state,
                            schemas,
                        )
                        .await
                        .map_err(AppError::Provider)?;
                        progress.finish();
                        Ok((id, refreshed))
                    });
                }

                if orphan_in_flight.is_empty() {
                    break;
                }

                let draining = refresh_cancelled;
                #[cfg(test)]
                let next_in_flight = async {
                    if draining {
                        crate::commands::shared::cancellation_test_support::observe_refresh_drain_wait(
                            &cancel,
                        )
                        .await;
                    }
                    orphan_in_flight.next().await
                };
                #[cfg(not(test))]
                let next_in_flight = orphan_in_flight.next();
                // A ready provider result is harvested before cleanup priority. If
                // refresh is already cancelled, the short-circuit below discards it.
                let next = cleanup_wait.until_cleanup_priority(next_in_flight);
                tokio::pin!(next);
                let next = if draining {
                    next.await
                } else {
                    tokio::select! {
                        biased;
                        next = &mut next => next,
                        _ = cancel.cancelled() => {
                            refresh_cancelled = true;
                            continue;
                        }
                    }
                };
                let result: Result<(ResourceId, carina_core::differ::Orphan), AppError> = match next {
                    CleanupInterrupted::Completed(Some(result)) => result,
                    CleanupInterrupted::Completed(None) => break,
                    CleanupInterrupted::Abandoned => {
                        refresh_cancelled = true;
                        break;
                    }
                };

                if refresh_cancelled {
                    continue;
                }
                let (id, loaded) = result?;
                let current = loaded.into_current();
                let exists = match &current {
                    Current::Absent(_state) => false,
                    Current::Present(_present) => true,
                    Current::CreateInFlight(_pending) => true,
                    Current::MissingIdentifier(_state) => true,
                };
                if exists
                    && let Some(resolved_id) = ResolvedResourceId::try_new(id.clone())
                {
                    let orphan_resource = build_orphan_resource(sf, &resolved_id);
                    all_resources.push(orphan_resource);
                }
                managed_current.insert(id, current);
            }
            if refresh_cancelled {
                return Err(AppError::Interrupted);
            }
        }
    } else if let Some(sf) = state_file.as_ref() {
        // --refresh=false: build states from state file without AWS calls.
        // carina#3181: `all_resources` is managed-only.
        for resource in &all_resources {
            let current = sf.build_state_for_resource(&resource.id);
            managed_current.insert(resource.id.clone(), current);
        }

        // Include orphaned resources (in state but not in .crn)
        let desired_ids: HashSet<ResourceId> = all_resources.iter().map(|r| r.id.clone()).collect();
        for (id, loaded) in sf.build_orphans(&desired_ids) {
            let current = loaded.into_current();
            if let Some(resolved_id) = ResolvedResourceId::try_new(id.clone()) {
                let orphan_resource = build_orphan_resource(sf, &resolved_id);
                all_resources.push(orphan_resource);
            }
            managed_current.insert(id, current);
        }
    }

    // Collect resources that exist and will be destroyed
    // Skip the state bucket if it matches the backend bucket
    let mut protected_resources: Vec<&Resource> = Vec::new();
    let mut prevent_destroy_resources: Vec<&Resource> = Vec::new();
    let mut resources_to_destroy: Vec<&Resource> = all_resources
        .iter()
        .filter(|r| {
            // carina#3181: `all_resources` is managed-only — data
            // sources and compositions never enter the destroy set.
            let exists = managed_current
                .get(&r.id)
                .is_some_and(|current| match current {
                    Current::Absent(_state) => false,
                    Current::Present(_present) => true,
                    Current::CreateInFlight(_pending) => true,
                    Current::MissingIdentifier(_state) => true,
                });
            if !exists {
                return false;
            }

            // Check prevent_destroy directive (unless --force)
            if !force && r.directives.prevent_destroy {
                prevent_destroy_resources.push(r);
                return false;
            }

            // Check if this is the protected state bucket
            if let Some(backend_rt) = backend.resource_type()
                && r.id.resource_type == backend_rt
                && let Some(ref bucket_name) = protected_bucket
                && let Some(Value::Concrete(ConcreteValue::String(name))) = r.get_attr("bucket")
                && name == bucket_name
            {
                protected_resources.push(r);
                return false;
            }

            true
        })
        .collect();
    resources_to_destroy.sort_by(|left, right| {
        let left_key = left
            .binding
            .clone()
            .unwrap_or_else(|| left.id.identity_display().to_string());
        let right_key = right
            .binding
            .clone()
            .unwrap_or_else(|| right.id.identity_display().to_string());
        left_key.cmp(&right_key)
    });

    // Backend-bucket protection shields the whole state row, including deposed
    // generations, because deleting any generation could destroy the state
    // bucket itself. `prevent_destroy` is intentionally not included here: it
    // guards the current instance, while a deposed generation already came from
    // a replacement path that approved deleting the old instance.
    let protected_row_keys: HashSet<StateRowKey> = protected_resources
        .iter()
        .filter_map(|resource| ResolvedResourceId::try_new(resource.id.clone()))
        .map(|id| state_row_key_from_id(&id))
        .collect();
    let (delete_effects, identifier_errors) = build_destroy_delete_effects(
        &resources_to_destroy,
        &managed_current,
        state_file.as_ref(),
        &protected_row_keys,
    );
    if !identifier_errors.is_empty() {
        let mut error_plan = Plan::new();
        for error in identifier_errors {
            error_plan.add_error(error);
        }
        render_plan_errors_and_abort(&error_plan)?;
    }

    let wait_aliases = build_destroy_wait_aliases(&parsed.wait_bindings, &delete_effects);
    let dependency_analysis = build_effect_dependency_analysis(
        &delete_effects,
        &HashMap::<ResourceId, UnresolvedResource>::new(),
        &[],
        ScheduleInputs::Destroy {
            aliases: &wait_aliases,
        },
    )
    .map_err(DestroyDependencyAnalysisError::new)?;
    let delete_count = delete_effects.len();
    let deletion_deps = transitive_delete_deps(&dependency_analysis, delete_count);
    let delete_depths = compute_delete_depths(delete_count, &deletion_deps);
    let resource_names: Vec<String> = delete_effects
        .iter()
        .map(|effect| {
            effect
                .binding_name()
                .unwrap_or_else(|| effect.resource_id().identity_display().to_string())
        })
        .collect();
    let display_order = topological_delete_order(
        delete_count,
        &deletion_deps,
        &delete_depths,
        &resource_names,
    )
    .map_err(AppError::Config)?;

    if delete_effects.is_empty()
        && protected_resources.is_empty()
        && prevent_destroy_resources.is_empty()
    {
        println!("{}", "No resources to destroy.".green());
        return Ok(DestroyAfterHeartbeat::Done);
    }

    // Build a Plan from the delete effects for tree display
    let mut destroy_plan = Plan::new();
    for idx in &display_order {
        destroy_plan.add(delete_effects[*idx].clone());
    }

    let historical_display =
        collect_historical_display_attributes(&destroy_plan, &managed_current, state_file.as_ref());

    // Display destroy plan as a dependency tree
    print!(
        "{}",
        format_destroy_plan_with_delete_instances(
            &destroy_plan,
            DetailLevel::Full,
            &historical_display
        )
    );

    // Show protected resources
    for resource in &protected_resources {
        println!(
            "  {} {} {}",
            "⚠".yellow().bold(),
            resource.id,
            "(protected - will be skipped)".yellow()
        );
    }

    // Show prevent_destroy resources
    if !prevent_destroy_resources.is_empty() {
        println!();
        println!(
            "{}",
            "Error: the following resources have prevent_destroy set and cannot be destroyed:"
                .red()
                .bold()
        );
        let failure_progress = ProgressBackend::<()>::new(ProgressSink::Human);
        for resource in &prevent_destroy_resources {
            failure_progress.failed_line(FailedOutcome::summary(resource.id.to_string()));
        }
        println!();
        println!(
            "{}",
            "Use --force to override prevent_destroy and destroy these resources.".yellow()
        );
    }

    println!();
    let counts = destroy_plan_counts(
        delete_effects.len(),
        protected_resources.len(),
        prevent_destroy_resources.len(),
    );
    if counts.guarded > 0 {
        println!(
            "Plan: {} to destroy, {} protected.",
            counts.to_destroy.to_string().red(),
            counts.guarded.to_string().yellow()
        );
    } else {
        println!("Plan: {} to destroy.", counts.to_destroy.to_string().red());
    }
    println!();

    // If there are prevent_destroy resources, refuse to proceed
    if !prevent_destroy_resources.is_empty() {
        return Err(AppError::Validation(format!(
            "{} resource(s) have prevent_destroy set. Use --force to override.",
            prevent_destroy_resources.len()
        )));
    }

    if should_skip_destroy_execution(delete_effects.len()) {
        println!(
            "{}",
            "All resources are protected. Nothing to destroy.".yellow()
        );
        return Ok(DestroyAfterHeartbeat::Done);
    }

    // Confirmation prompt
    if !auto_approve {
        println!(
            "{}",
            "Do you really want to destroy all resources?"
                .yellow()
                .bold()
        );
        println!(
            "  {}",
            "This action cannot be undone. Type 'yes' to confirm.".yellow()
        );
        // The cursor is hidden command-wide (#3158); reveal it for this
        // irreversible-destroy confirmation so the user does not type
        // blind, and re-hide on scope exit.
        let mut input = String::new();
        {
            let _reveal = CursorReveal::new();
            print!("\n  Enter a value: ");
            std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

            std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| e.to_string())?;
        }

        if input.trim() != "yes" {
            println!();
            println!("{}", "Destroy cancelled.".yellow());
            return Ok(DestroyAfterHeartbeat::Done);
        }
        println!();
    }

    println!("{}", "Destroying resources...".red().bold());
    println!();

    let progress = ProgressBackend::new(ProgressSink::Human);

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut unknown_count = 0;
    let saver = IncrementalStateSaver::start(
        state_file.unwrap_or_default(),
        mid_run_writer,
        |mut state, checkpoint: &Vec<DestroyedInstance>| {
            apply_destroy_to_state(
                &mut state,
                checkpoint,
                DestroyStateWrite::Checkpoint,
            );
            Ok(state)
        },
    );
    let mut destroyed_instances = DestroyedInstances::new(saver.publisher());
    let mut failed_indices: HashSet<usize> = HashSet::new();
    let mut cancelled = false;

    let destroy_total = delete_effects.len();
    let completed_counter = AtomicUsize::new(0);

    // Pre-compute binding and effect for each resource by index
    let resource_info: Vec<(String, Effect)> = delete_effects
        .iter()
        .map(|effect| {
            let binding = effect.binding_name().unwrap_or_else(|| {
                let id = effect.resource_id();
                format!("{}:{}", id.resource_type, id.identity_display())
            });
            (binding, effect.clone())
        })
        .collect();

    // Track completed and dispatched indices
    let mut completed_indices: HashSet<usize> = HashSet::new();
    let mut dispatched: HashSet<usize> = HashSet::new();
    let all_indices = display_order.clone();

    // Track retry counts for dependency-violation retries
    let max_retries: usize = 3;
    let mut retry_counts: HashMap<usize, usize> = HashMap::new();
    // Indices waiting for at least one other effect to complete before retrying.
    // They are moved back to the ready pool when `in_flight.next()` returns.
    let mut retry_pending: HashSet<usize> = HashSet::new();

    let mut in_flight = FuturesUnordered::new();
    let cleanup_loop = cancel.cleanup_aware_loop();

    'destroy: loop {
        let undispatched_at_loop_start = all_indices
            .iter()
            .filter(|&&idx| !dispatched.contains(&idx))
            .count();
        let cleanup_wait = match cleanup_loop.step() {
            LoopStep::Continue(wait) => wait,
            LoopStep::Abandon => {
                cancelled = true;
                break 'destroy;
            }
        };
        match cleanup_wait.phase() {
            LoopShutdownPhase::Graceful
                if !cancelled && (undispatched_at_loop_start > 0 || !in_flight.is_empty()) =>
            {
                cancelled = true;
            }
            LoopShutdownPhase::Running | LoopShutdownPhase::Graceful => {}
        }

        if !cancelled {
            // Find newly ready resources: all deletion deps completed, not yet
            // dispatched, and not waiting for a retry gate.
            let mut newly_ready: Vec<usize> = Vec::new();
            for &idx in &all_indices {
                if dispatched.contains(&idx) || retry_pending.contains(&idx) {
                    continue;
                }
                let deps = &deletion_deps[&idx];
                if deps.iter().all(|d| completed_indices.contains(d)) {
                    newly_ready.push(idx);
                }
            }
            newly_ready.truncate(parallelism.get().saturating_sub(in_flight.len()));

            // Process newly ready resources
            for idx in newly_ready {
                match cancel.phase() {
                    ShutdownPhase::Running => {}
                    ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
                        cancelled = true;
                        break;
                    }
                }

                dispatched.insert(idx);

                let (_binding, effect) = &resource_info[idx];

                // Check if any dependent has failed.
                if let Some(failed_dep_idx) = delete_dependency_in_set(
                    idx,
                    &dependency_analysis,
                    delete_count,
                    &failed_indices,
                ) {
                    let failed_dep = &resource_info[failed_dep_idx].0;
                    let outcome = NonFailureOutcome::skipped(format_destroy_outcome_summary(
                        &completed_counter,
                        destroy_total,
                        effect,
                        format!("- skipped (dependent {failed_dep} failed)"),
                    ));
                    progress.outcome_line(outcome);
                    skip_count += 1;
                    failed_indices.insert(idx);
                    completed_indices.insert(idx);
                    continue;
                }

                let active_progress = progress.start_indexed(idx, format_effect(effect));

                // Spawn the deletion as a concurrent future
                let Effect::Delete {
                    id,
                    identifier,
                    generation,
                    directives,
                    ..
                } = effect
                else {
                    unreachable!("destroy dispatch only contains delete effects");
                };
                let resource_id = id.clone();
                let identifier = identifier.clone();
                let generation = generation.clone();
                let directives = directives.clone();

                let provider_ref = &provider;
                let result_shutdown = &cancel;
                let delete = async move {
                    let started = Instant::now();
                    let delete_result = provider_ref
                        .delete(
                            resource_id.as_inner(),
                            &identifier,
                            carina_core::provider::DeleteRequest {
                                directives: directives.clone(),
                            },
                        )
                        .await;
                    observe_destroy_result_ready_for_tests(result_shutdown).await;
                    (resource_id, generation, started, delete_result)
                };
                in_flight.push(DestroyInFlight::new(idx, active_progress, delete));
            }
        }

        // If nothing is in flight, we're done (or stuck)
        if in_flight.is_empty() {
            if cancelled {
                let remaining: Vec<usize> = all_indices
                    .iter()
                    .filter(|idx| !dispatched.contains(idx) && !completed_indices.contains(idx))
                    .copied()
                    .collect();
                for idx in remaining {
                    let (_, effect) = &resource_info[idx];
                    let outcome = NonFailureOutcome::skipped(format_destroy_outcome_summary(
                        &completed_counter,
                        destroy_total,
                        effect,
                        "- skipped (destroy cancelled)",
                    ));
                    progress.outcome_line(outcome);
                    dispatched.insert(idx);
                    completed_indices.insert(idx);
                    skip_count += 1;
                }
                break;
            }
            let remaining: Vec<usize> = all_indices
                .iter()
                .filter(|idx| !dispatched.contains(idx) && !completed_indices.contains(idx))
                .copied()
                .collect();
            if remaining.is_empty() {
                break;
            }
            // Check if all remaining are retry-pending items (deadlock: no
            // progress possible because every pending item needs something
            // else to complete first, but nothing else is running).
            let all_retried = remaining.iter().all(|idx| retry_counts.contains_key(idx));
            if all_retried {
                for &idx in &remaining {
                    let (_, effect) = &resource_info[idx];
                    let outcome = FailedOutcome::summary(format_destroy_outcome_summary(
                        &completed_counter,
                        destroy_total,
                        effect,
                        "- retries exhausted (no progress possible)",
                    ));
                    progress.failed_line(outcome);
                    failed_indices.insert(idx);
                    dispatched.insert(idx);
                    completed_indices.insert(idx);
                    failure_count += 1;
                }
                break;
            }
            // Non-retry cycle: fail every remaining outcome.
            for &idx in &remaining {
                let (_, effect) = &resource_info[idx];
                let outcome = FailedOutcome::summary(format_destroy_outcome_summary(
                    &completed_counter,
                    destroy_total,
                    effect,
                    "- dependency cycle (no progress possible)",
                ));
                progress.failed_line(outcome);
                dispatched.insert(idx);
                completed_indices.insert(idx);
                failure_count += 1;
            }
            break;
        }

        // Wait for the next deletion to complete. A ready provider result is
        // harvested before cleanup priority, matching the scheduler contract.
        let next = {
            let next = cleanup_wait.until_cleanup_priority(in_flight.next());
            tokio::pin!(next);
            if cancelled {
                Some(next.await)
            } else {
                tokio::select! {
                    biased;
                    next = &mut next => Some(next),
                    _ = cancel.cancelled() => None,
                }
            }
        };
        let Some(next) = next else {
            cancelled = true;
            continue;
        };
        let (
            finished_idx,
            active_progress,
            (resource_id, generation, started, delete_result),
        ) = match next {
            CleanupInterrupted::Completed(Some(finished)) => finished,
            CleanupInterrupted::Completed(None) => break 'destroy,
            CleanupInterrupted::Abandoned => {
                cancelled = true;
                break 'destroy;
            }
        };
        completed_indices.insert(finished_idx);

        // An effect completed — release all retry-pending indices so they
        // become eligible in the next iteration's ready-check.
        retry_pending.clear();

        let effect = &resource_info[finished_idx].1;

        match delete_result {
            Ok(()) => {
                let timing = format!("took {}", format_duration(started.elapsed())).dimmed();
                let outcome = NonFailureOutcome::success(format_destroy_outcome_summary(
                    &completed_counter,
                    destroy_total,
                    effect,
                    timing,
                ));
                active_progress.finish(outcome);
                success_count += 1;
                observe_destroy_success_for_tests(success_count, &resource_id, &cancel);
                let destroyed = DestroyedInstance {
                    id: resource_id,
                    generation,
                };
                destroyed_instances.record(destroyed);
            }
            Err(e) => {
                let retries = retry_counts.get(&finished_idx).copied().unwrap_or(0);
                let has_pending_or_in_flight = !in_flight.is_empty()
                    || all_indices
                        .iter()
                        .any(|idx| !dispatched.contains(idx) && !completed_indices.contains(idx));
                if is_retryable_delete_error(&e)
                    && retries < max_retries
                    && has_pending_or_in_flight
                {
                    *retry_counts.entry(finished_idx).or_insert(0) += 1;
                    completed_indices.remove(&finished_idx);
                    dispatched.remove(&finished_idx);
                    retry_pending.insert(finished_idx);
                    let retry_num = retry_counts[&finished_idx];
                    let outcome = NonFailureOutcome::retrying(format!(
                        "{} - dependency violation, will retry ({}/{})",
                        format_effect(effect),
                        retry_num,
                        max_retries
                    ));
                    active_progress.finish(outcome);
                } else {
                    let timing = format!("took {}", format_duration(started.elapsed())).dimmed();
                    let outcome = FailedOutcome::new(
                        format_destroy_outcome_summary(
                            &completed_counter,
                            destroy_total,
                            effect,
                            timing,
                        ),
                        [e.to_string()],
                    );
                    active_progress.finish_failed(outcome);
                    failure_count += 1;
                    failed_indices.insert(finished_idx);
                }
            }
        }
    }

    // Cleanup-priority abandonment can exit before the normal cancellation
    // drain gets another loop iteration. Finish both active spinners and
    // never-dispatched resources before their backend is dropped.
    let mut active_progress: HashMap<_, _> = in_flight
        .into_iter()
        .map(DestroyInFlight::into_progress)
        .collect();
    if all_indices
        .iter()
        .any(|idx| !completed_indices.contains(idx))
    {
        cancelled = true;
        let interrupted = finish_interrupted_destroy_outcomes(
            &progress,
            &mut active_progress,
            &all_indices,
            &completed_indices,
            &dispatched,
            |idx, outcome| {
                let effect = &resource_info[idx].1;
                match outcome {
                    InterruptedDestroyOutcome::Skipped => NonFailureOutcome::skipped(
                        format_destroy_outcome_summary(
                            &completed_counter,
                            destroy_total,
                            effect,
                            "- skipped (destroy cancelled)",
                        ),
                    ),
                    InterruptedDestroyOutcome::Unknown => NonFailureOutcome::unknown(
                        format_destroy_outcome_summary(
                            &completed_counter,
                            destroy_total,
                            effect,
                            "- interrupted (outcome unknown)",
                        ),
                    ),
                }
            },
        );
        skip_count += interrupted.skipped;
        unknown_count += interrupted.unknown;
    }
    let state_base = saver.stop(&cancel).await;
    let destroyed_instances = destroyed_instances.into_instances();

    Ok(DestroyAfterHeartbeat::Executed(Box::new(DestroyExecuted {
        state_base,
        destroyed_instances,
        success_count,
        failure_count,
        skip_count,
        unknown_count,
        cancelled,
        cancel,
    })))
        },
    )
    .await;
    let (heartbeat_result, lock) = match heartbeat {
        HeartbeatOutcome::Kept { result, lock } => (result, lock),
        HeartbeatOutcome::Lost {
            result: _result,
            lost,
        } => {
            return LockCommandOutcome::Lost(AppError::StateLockLost {
                error: lost.into_error(),
            });
        }
    };

    let op_result = async {
        match heartbeat_result? {
            DestroyAfterHeartbeat::Done => Ok(()),
            DestroyAfterHeartbeat::Executed(context) => {
                let DestroyExecuted {
                    state_base,
                    destroyed_instances,
                    success_count,
                    failure_count,
                    skip_count,
                    unknown_count,
                    cancelled,
                    cancel,
                } = *context;
                if cancelled && destroyed_instances.is_empty() {
                    println!();
                    println!(
                        "{}",
                        format_destroy_interrupted_summary(
                            success_count,
                            failure_count,
                            skip_count,
                            unknown_count,
                        )
                        .yellow()
                        .bold()
                    );
                    return Err(AppError::Interrupted);
                }

                println!();
                println!("{}", "Saving state...".cyan());

                let finalize_result = finalize_after_execute(
                    |persistence| {
                        finalize_destroy(
                            FinalizeDestroyInput {
                                backend: backend.as_ref(),
                                lock: lock.as_ref(),
                                state_base,
                                destroyed_instances: &destroyed_instances,
                            },
                            persistence,
                        )
                    },
                    cancelled,
                    &cancel,
                )
                .await;

                if let Err(error) = finalize_result {
                    if cancelled {
                        println!();
                        println!(
                            "{}",
                            format_destroy_interrupted_summary(
                                success_count,
                                failure_count,
                                skip_count,
                                unknown_count,
                            )
                            .yellow()
                            .bold()
                        );
                    }
                    return Err(error);
                }

                println!();
                if failure_count == 0 && skip_count == 0 && unknown_count == 0 {
                    println!(
                        "{}",
                        format!("Destroy complete! {} resources destroyed.", success_count)
                            .green()
                            .bold()
                    );
                    Ok(())
                } else {
                    Err(AppError::Config(format_destroy_failure_summary(
                        success_count,
                        failure_count,
                        skip_count,
                        unknown_count,
                    )))
                }
            }
        }
    }
    .await;

    LockCommandOutcome::Kept {
        result: op_result,
        lock,
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_destroy_locked(
    parsed: carina_core::parser::InferredFile,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    protected_bucket: Option<String>,
    lock: StateLock,
    refresh: bool,
    force: bool,
    base_dir: &Path,
    provider_context: &ProviderContext,
    parallelism: NonZeroUsize,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    let user_dirs = carina_core::TestUserDirs::new();
    let providers = match ProviderLoad::load(&user_dirs, &parsed.providers, base_dir) {
        Ok(providers) => providers,
        Err(error) => {
            return finish_lock_command(
                backend.as_ref(),
                LockCommandOutcome::Kept {
                    result: Err(error),
                    lock,
                },
                &cancel,
            )
            .await;
        }
    };
    let validated_config =
        match providers.validate_evaluated_config(parsed, base_dir, true, &[], &[]) {
            Ok(validated_config) => validated_config,
            Err(error) => {
                return finish_lock_command(
                    backend.as_ref(),
                    LockCommandOutcome::Kept {
                        result: Err(error),
                        lock,
                    },
                    &cancel,
                )
                .await;
            }
        };
    let outcome = run_destroy_locked_owned(
        validated_config,
        auto_approve,
        Arc::clone(&backend),
        protected_bucket,
        lock,
        refresh,
        force,
        base_dir,
        provider_context,
        parallelism,
        cancel.clone(),
    )
    .await;
    finish_lock_command(backend.as_ref(), outcome, &cancel).await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_destroy_locked_with_ctx(
    validated_config: ValidatedConfig<carina_core::parser::InferredFile>,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    protected_bucket: Option<String>,
    lock: StateLock,
    refresh: bool,
    force: bool,
    base_dir: &Path,
    provider_context: &ProviderContext,
    parallelism: NonZeroUsize,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    let outcome = run_destroy_locked_owned(
        validated_config,
        auto_approve,
        Arc::clone(&backend),
        protected_bucket,
        lock,
        refresh,
        force,
        base_dir,
        provider_context,
        parallelism,
        cancel.clone(),
    )
    .await;
    finish_lock_command(backend.as_ref(), outcome, &cancel).await
}

#[cfg(test)]
static DESTROY_SUCCESS_CANCEL_AFTER: std::sync::Mutex<Option<DestroySuccessCancelHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static DESTROY_READY_RESULT_BARRIER: std::sync::Mutex<Option<DestroyReadyResultBarrier>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct DestroyReadyResultBarrier {
    reached: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
    shutdown: ShutdownToken,
}

#[cfg(test)]
impl DestroyReadyResultBarrier {
    fn new(shutdown: ShutdownToken) -> Self {
        Self {
            reached: std::sync::Arc::new(tokio::sync::Notify::new()),
            release: std::sync::Arc::new(tokio::sync::Notify::new()),
            shutdown,
        }
    }

    pub(crate) async fn reached(&self) {
        self.reached.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

#[cfg(test)]
struct DestroySuccessCancelHook {
    threshold: usize,
    trigger: TestShutdownTrigger,
    shutdown: ShutdownToken,
}

#[cfg(test)]
pub(crate) fn set_destroy_success_cancel_after(
    threshold: usize,
    trigger: TestShutdownTrigger,
    shutdown: ShutdownToken,
) {
    *DESTROY_SUCCESS_CANCEL_AFTER
        .lock()
        .expect("destroy success cancel hook lock") = Some(DestroySuccessCancelHook {
        threshold,
        trigger,
        shutdown,
    });
}

#[cfg(test)]
pub(crate) fn clear_destroy_success_cancel_after() {
    *DESTROY_SUCCESS_CANCEL_AFTER
        .lock()
        .expect("destroy success cancel hook lock") = None;
}

#[cfg(test)]
pub(crate) fn install_destroy_ready_result_barrier(
    shutdown: ShutdownToken,
) -> DestroyReadyResultBarrier {
    let barrier = DestroyReadyResultBarrier::new(shutdown);
    *DESTROY_READY_RESULT_BARRIER
        .lock()
        .expect("destroy ready result barrier lock") = Some(barrier.clone());
    barrier
}

#[cfg(test)]
pub(crate) fn clear_destroy_ready_result_barrier() {
    *DESTROY_READY_RESULT_BARRIER
        .lock()
        .expect("destroy ready result barrier lock") = None;
}

#[cfg(test)]
async fn observe_destroy_result_ready_for_tests(shutdown: &ShutdownToken) {
    let barrier = {
        let mut barrier = DESTROY_READY_RESULT_BARRIER
            .lock()
            .expect("destroy ready result barrier lock");
        if barrier.as_ref().is_some_and(|barrier| {
            carina_core::shutdown::testing::same_shutdown_channel(&barrier.shutdown, shutdown)
        }) {
            barrier.take()
        } else {
            None
        }
    };
    if let Some(barrier) = barrier {
        barrier.reached.notify_one();
        barrier.release.notified().await;
    }
}

#[cfg(not(test))]
async fn observe_destroy_result_ready_for_tests(_shutdown: &ShutdownToken) {}

#[cfg(test)]
fn observe_destroy_success_for_tests(
    success_count: usize,
    _resource_id: &ResourceId,
    cancel: &ShutdownToken,
) {
    if let Some(hook) = DESTROY_SUCCESS_CANCEL_AFTER
        .lock()
        .expect("destroy success cancel hook lock")
        .as_ref()
        && carina_core::shutdown::testing::same_shutdown_channel(&hook.shutdown, cancel)
        && success_count == hook.threshold
    {
        hook.trigger.request_graceful_shutdown();
    }
}

#[cfg(not(test))]
fn observe_destroy_success_for_tests(
    _success_count: usize,
    _resource_id: &ResourceId,
    _cancel: &ShutdownToken,
) {
}

fn build_destroy_wait_aliases(
    wait_bindings: &[WaitBinding],
    delete_effects: &[Effect],
) -> Vec<DestroyWaitAlias> {
    let delete_dependencies: Vec<(String, HashSet<String>)> = delete_effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Delete {
                id, dependencies, ..
            } => Some((
                effect
                    .binding_name()
                    .unwrap_or_else(|| id.identity_str().to_string()),
                dependencies.clone(),
            )),
            _ => None,
        })
        .collect();
    let destroy_targets: HashSet<&str> = delete_dependencies
        .iter()
        .map(|(binding, _)| binding.as_str())
        .collect();
    let referenced_bindings: HashSet<&str> = delete_dependencies
        .iter()
        .flat_map(|(_, dependencies)| dependencies.iter().map(String::as_str))
        .collect();

    wait_bindings
        .iter()
        .filter(|wait| referenced_bindings.contains(wait.binding.as_str()))
        .filter_map(|wait| {
            if !destroy_targets.contains(wait.target.as_str()) {
                return None;
            }
            let mut consumers = delete_dependencies
                .iter()
                .filter(|(_, dependencies)| dependencies.contains(wait.binding.as_str()))
                .map(|(binding, _)| binding.clone())
                .collect::<Vec<_>>();
            consumers.sort();
            consumers.dedup();

            DestroyWaitAlias::new(
                wait.binding.as_str().to_string(),
                wait.target.as_str().to_string(),
                wait.depends_on
                    .iter()
                    .map(|dep| dep.as_str().to_string())
                    .collect(),
                consumers,
            )
        })
        .collect()
}

fn build_destroy_delete_effects(
    resources_to_destroy: &[&Resource],
    managed_current: &HashMap<ResourceId, Current>,
    state_file: Option<&StateFile>,
    protected_row_keys: &HashSet<StateRowKey>,
) -> (Vec<Effect>, Vec<PlanError>) {
    let mut delete_effects = Vec::with_capacity(resources_to_destroy.len());
    let mut errors = Vec::new();
    for resource in resources_to_destroy {
        let identifier = match managed_current.get(&resource.id) {
            Some(Current::Present(present)) => present.identifier().clone(),
            Some(Current::Absent(_state)) => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    carina_core::plan::PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
            Some(Current::MissingIdentifier(_state)) => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    carina_core::plan::PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
            Some(Current::CreateInFlight(_pending)) => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    carina_core::plan::PlanErrorKind::DestroyCreateInFlight,
                ));
                continue;
            }
            None => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    carina_core::plan::PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
        };
        let dependencies = get_resource_dependencies(resource);
        let explicit_dependencies = resource.directives.depends_on.iter().cloned().collect();
        delete_effects.push(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(resource.id.clone()),
            identifier,
            generation: EffectGeneration::Current,
            directives: resource.directives.clone(),
            binding: resource.binding.clone(),
            dependencies,
            explicit_dependencies,
            blocked_by_updates: HashSet::new(),
        });
    }
    if let Some(sf) = state_file {
        for row in sf.resources() {
            if protected_row_keys.contains(&state_row_key_from_parts(
                &row.provider,
                &row.resource_type,
                row.identity.as_str(),
            )) {
                continue;
            }
            let (effects, row_errors) = crate::wiring::deposed_delete_effects_for_row(row, &[]);
            delete_effects.extend(effects);
            errors.extend(row_errors);
        }
    }
    (delete_effects, errors)
}

type StateRowKey = (String, String, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DestroyPlanCounts {
    to_destroy: usize,
    guarded: usize,
}

fn destroy_plan_counts(
    delete_effect_count: usize,
    protected_count: usize,
    prevent_destroy_count: usize,
) -> DestroyPlanCounts {
    DestroyPlanCounts {
        to_destroy: delete_effect_count,
        guarded: protected_count + prevent_destroy_count,
    }
}

fn should_skip_destroy_execution(delete_effect_count: usize) -> bool {
    delete_effect_count == 0
}

fn format_destroy_interrupted_summary(
    success_count: usize,
    failure_count: usize,
    skip_count: usize,
    unknown_count: usize,
) -> String {
    format!(
        "Destroy interrupted. {success_count} succeeded, {failure_count} failed, \
         {skip_count} skipped, {unknown_count} outcome{} unknown.",
        if unknown_count == 1 { "" } else { "s" }
    )
}

fn format_destroy_failure_summary(
    success_count: usize,
    failure_count: usize,
    skip_count: usize,
    unknown_count: usize,
) -> String {
    if unknown_count == 0 {
        return format!(
            "Destroy failed. {success_count} succeeded, {failure_count} failed, \
             {skip_count} skipped."
        );
    }
    format!(
        "Destroy failed. {success_count} succeeded, {failure_count} failed, \
         {skip_count} skipped, {unknown_count} outcome{} unknown.",
        if unknown_count == 1 { "" } else { "s" }
    )
}

fn format_destroy_outcome_summary(
    completed_counter: &AtomicUsize,
    total: usize,
    effect: &Effect,
    detail: impl std::fmt::Display,
) -> String {
    let completed = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let counter = format!("{completed}/{total}").dimmed();
    format!("{} {} {}", format_effect(effect), detail, counter)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptedDestroyOutcome {
    Skipped,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct InterruptedDestroyCounts {
    skipped: usize,
    unknown: usize,
}

fn finish_interrupted_destroy_outcomes(
    progress: &ProgressBackend<usize>,
    active_progress: &mut HashMap<usize, ProgressGuard<'_, usize>>,
    all_indices: &[usize],
    completed_indices: &HashSet<usize>,
    dispatched: &HashSet<usize>,
    mut outcome_for: impl FnMut(usize, InterruptedDestroyOutcome) -> NonFailureOutcome,
) -> InterruptedDestroyCounts {
    let mut counts = InterruptedDestroyCounts::default();
    for &idx in all_indices {
        if completed_indices.contains(&idx) {
            continue;
        }
        let kind = if dispatched.contains(&idx) {
            counts.unknown += 1;
            InterruptedDestroyOutcome::Unknown
        } else {
            counts.skipped += 1;
            InterruptedDestroyOutcome::Skipped
        };
        let outcome = outcome_for(idx, kind);
        if kind == InterruptedDestroyOutcome::Unknown {
            active_progress
                .remove(&idx)
                .expect("dispatched destroy effect must own progress")
                .finish(outcome);
        } else {
            progress.outcome_line(outcome);
        }
    }
    assert!(
        active_progress.is_empty(),
        "every in-flight destroy effect must receive an interrupted outcome"
    );
    counts
}

fn state_row_key_from_id(id: &ResolvedResourceId) -> StateRowKey {
    state_row_key_from_parts(&id.provider, &id.resource_type, id.identity_str())
}

fn state_row_key_from_parts(provider: &str, resource_type: &str, identity: &str) -> StateRowKey {
    (
        provider.to_string(),
        resource_type.to_string(),
        identity.to_string(),
    )
}

fn transitive_delete_deps(
    analysis: &DependencyAnalysis,
    delete_count: usize,
) -> HashMap<usize, HashSet<usize>> {
    (0..delete_count)
        .map(|idx| {
            let mut deps = HashSet::new();
            let mut seen = HashSet::new();
            collect_delete_deps(idx, analysis, delete_count, &mut seen, &mut deps);
            (idx, deps)
        })
        .collect()
}

fn collect_delete_deps(
    idx: usize,
    analysis: &DependencyAnalysis,
    delete_count: usize,
    seen: &mut HashSet<usize>,
    deps: &mut HashSet<usize>,
) {
    let Some(children) = analysis.deps_of(idx) else {
        return;
    };

    for &child in children {
        if !seen.insert(child) {
            continue;
        }
        if child < delete_count {
            deps.insert(child);
        } else {
            collect_delete_deps(child, analysis, delete_count, seen, deps);
        }
    }
}

fn topological_delete_order(
    delete_count: usize,
    deletion_deps: &HashMap<usize, HashSet<usize>>,
    depths: &[usize],
    names: &[String],
) -> Result<Vec<usize>, String> {
    let mut emitted = HashSet::new();
    let mut order = Vec::with_capacity(delete_count);

    while order.len() < delete_count {
        let ready = (0..delete_count)
            .filter(|idx| {
                !emitted.contains(idx)
                    && deletion_deps
                        .get(idx)
                        .is_none_or(|deps| deps.iter().all(|dep| emitted.contains(dep)))
            })
            .max_by(|left, right| {
                depths[*left]
                    .cmp(&depths[*right])
                    .then_with(|| names[*right].cmp(&names[*left]))
                    .then_with(|| right.cmp(left))
            });

        let Some(idx) = ready else {
            return Err(format!(
                "Circular dependency detected: {}",
                cycle_path(delete_count, deletion_deps, names)
            ));
        };

        emitted.insert(idx);
        order.push(idx);
    }

    Ok(order)
}

fn compute_delete_depths(
    delete_count: usize,
    deletion_deps: &HashMap<usize, HashSet<usize>>,
) -> Vec<usize> {
    let mut create_parents: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (&parent, children) in deletion_deps {
        for &child in children {
            create_parents.entry(child).or_default().insert(parent);
        }
    }

    let mut memo = HashMap::new();
    (0..delete_count)
        .map(|idx| compute_delete_depth(idx, &create_parents, &mut memo, &mut HashSet::new()))
        .collect()
}

fn compute_delete_depth(
    idx: usize,
    create_parents: &HashMap<usize, HashSet<usize>>,
    memo: &mut HashMap<usize, usize>,
    visiting: &mut HashSet<usize>,
) -> usize {
    if let Some(depth) = memo.get(&idx) {
        return *depth;
    }
    if !visiting.insert(idx) {
        return 0;
    }
    let depth = create_parents
        .get(&idx)
        .map(|parents| {
            parents
                .iter()
                .map(|parent| compute_delete_depth(*parent, create_parents, memo, visiting) + 1)
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    visiting.remove(&idx);
    memo.insert(idx, depth);
    depth
}

fn cycle_path(
    delete_count: usize,
    deletion_deps: &HashMap<usize, HashSet<usize>>,
    names: &[String],
) -> String {
    fn dfs(
        idx: usize,
        deletion_deps: &HashMap<usize, HashSet<usize>>,
        visiting: &mut Vec<usize>,
        visited: &mut HashSet<usize>,
    ) -> Option<Vec<usize>> {
        if let Some(pos) = visiting.iter().position(|node| *node == idx) {
            let mut path = visiting[pos..].to_vec();
            path.push(idx);
            return Some(path);
        }
        if !visited.insert(idx) {
            return None;
        }
        visiting.push(idx);
        if let Some(deps) = deletion_deps.get(&idx) {
            let mut sorted: Vec<_> = deps.iter().copied().collect();
            sorted.sort_unstable();
            for dep in sorted {
                if let Some(path) = dfs(dep, deletion_deps, visiting, visited) {
                    return Some(path);
                }
            }
        }
        visiting.pop();
        None
    }

    let mut visited = HashSet::new();
    for idx in 0..delete_count {
        if let Some(path) = dfs(idx, deletion_deps, &mut Vec::new(), &mut visited) {
            return path
                .into_iter()
                .map(|idx| names.get(idx).cloned().unwrap_or_else(|| idx.to_string()))
                .collect::<Vec<_>>()
                .join(" -> ");
        }
    }
    "<unknown>".to_string()
}

fn delete_dependency_in_set(
    idx: usize,
    analysis: &DependencyAnalysis,
    delete_count: usize,
    candidates: &HashSet<usize>,
) -> Option<usize> {
    let mut sorted: Vec<_> = candidates.iter().copied().collect();
    sorted.sort_unstable();
    sorted.into_iter().find(|candidate| {
        *candidate < delete_count && graph_reaches_delete_idx(*candidate, idx, analysis)
    })
}

fn graph_reaches_delete_idx(start: usize, target: usize, analysis: &DependencyAnalysis) -> bool {
    let mut stack = vec![start];
    let mut seen = HashSet::new();

    while let Some(idx) = stack.pop() {
        if !seen.insert(idx) {
            continue;
        }
        if idx == target {
            return true;
        }
        if let Some(dependents) = analysis.dependents_of(idx) {
            stack.extend(dependents.iter().copied());
        }
    }

    false
}

pub(crate) struct FinalizeDestroyInput<'a> {
    pub backend: &'a dyn StateBackend,
    pub lock: StateLockRef<'a>,
    pub state_base: FinalStateBase,
    pub destroyed_instances: &'a [DestroyedInstance],
}

pub(crate) async fn finalize_destroy(
    input: FinalizeDestroyInput<'_>,
    persistence: StatePersistence,
) -> Result<(), AppError> {
    let mut state = input.state_base.into_state();

    apply_destroy_to_state(
        &mut state,
        input.destroyed_instances,
        DestroyStateWrite::Final,
    );

    match input.lock {
        StateLockRef::Held(lock) => {
            crate::commands::apply::save_state_locked_after_execute(
                input.backend,
                lock,
                &mut state,
                persistence,
            )
            .await?;
        }
        StateLockRef::Disabled(disabled) => {
            crate::commands::apply::save_state_unlocked_after_execute(
                input.backend,
                disabled,
                &mut state,
                persistence,
            )
            .await?;
        }
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_upstream_test_support::{
        ProviderConfigCaptureFactory, provider_default_tags_local_resource_fixture,
        provider_default_tags_missing_upstream_fixture, provider_upstream_fixture,
    };
    use carina_core::provider::ProviderError;
    use std::sync::{Arc, Mutex};

    #[path = "cancellation_fixture.rs"]
    mod cancellation_fixture;

    use cancellation_fixture::DestroyCancellationFixture;

    #[derive(Clone, Default)]
    struct SharedProgressWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedProgressWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn interrupted_destroy_finishes_in_flight_and_undispatched_outcomes() {
        let output = SharedProgressWriter::default();
        let progress = ProgressBackend::plain(output.clone());
        let mut active_progress =
            HashMap::from([(0, progress.start_indexed(0, "Delete in-flight"))]);
        let completed = HashSet::from([2]);
        let dispatched = HashSet::from([0]);

        let counts = finish_interrupted_destroy_outcomes(
            &progress,
            &mut active_progress,
            &[0, 1, 2],
            &completed,
            &dispatched,
            |idx, outcome| match outcome {
                InterruptedDestroyOutcome::Unknown => NonFailureOutcome::unknown(format!(
                    "Delete resource-{idx} - interrupted (outcome unknown)"
                )),
                InterruptedDestroyOutcome::Skipped => NonFailureOutcome::skipped(format!(
                    "Delete resource-{idx} - skipped (destroy cancelled)"
                )),
            },
        );

        assert_eq!(
            counts,
            InterruptedDestroyCounts {
                skipped: 1,
                unknown: 1,
            }
        );
        assert_eq!(
            String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
            concat!(
                "  ? Delete resource-0 - interrupted (outcome unknown)\n",
                "  ⊘ Delete resource-1 - skipped (destroy cancelled)\n",
            )
        );
    }

    #[test]
    #[should_panic(expected = "dispatched destroy effect must own progress")]
    fn interrupted_destroy_requires_progress_for_dispatched_effect() {
        let output = SharedProgressWriter::default();
        let progress = ProgressBackend::plain(output);
        let mut active_progress = HashMap::new();

        finish_interrupted_destroy_outcomes(
            &progress,
            &mut active_progress,
            &[0],
            &HashSet::new(),
            &HashSet::from([0]),
            |idx, outcome| match outcome {
                InterruptedDestroyOutcome::Unknown => NonFailureOutcome::unknown(format!(
                    "Delete resource-{idx} - interrupted (outcome unknown)"
                )),
                InterruptedDestroyOutcome::Skipped => unreachable!(),
            },
        );
    }

    #[test]
    fn destroy_summaries_keep_unknown_outcomes_separate_from_skips() {
        assert_eq!(
            format_destroy_interrupted_summary(1, 2, 3, 1),
            "Destroy interrupted. 1 succeeded, 2 failed, 3 skipped, 1 outcome unknown."
        );
        assert_eq!(
            format_destroy_interrupted_summary(1, 2, 3, 2),
            "Destroy interrupted. 1 succeeded, 2 failed, 3 skipped, 2 outcomes unknown."
        );
        assert_eq!(
            format_destroy_failure_summary(1, 2, 3, 0),
            "Destroy failed. 1 succeeded, 2 failed, 3 skipped."
        );
    }

    #[test]
    fn destroyed_instances_owner_publishes_snapshots_and_derives_final_view() {
        let first = DestroyedInstance {
            id: ResolvedResourceId::with_provider_identity("mock", "test.resource", "first", None),
            generation: EffectGeneration::Current,
        };
        let second = DestroyedInstance {
            id: ResolvedResourceId::with_provider_identity("mock", "test.resource", "second", None),
            generation: EffectGeneration::Current,
        };
        let (publisher, receiver) = tokio::sync::watch::channel(None);
        let mut completed = DestroyedInstances::new(publisher);

        completed.record(first.clone());
        let checkpoint = receiver
            .borrow()
            .clone()
            .expect("recording a destroy must publish its checkpoint");
        completed.record(second.clone());

        assert_eq!(checkpoint.len(), 1);
        assert_eq!(checkpoint[0].id, first.id);

        let final_instances = completed.into_instances();
        assert_eq!(final_instances.len(), 2);
        assert_eq!(final_instances[0].id, first.id);
        assert_eq!(final_instances[1].id, second.id);
    }

    fn expect_valid_destroy_effects(result: (Vec<Effect>, Vec<PlanError>)) -> Vec<Effect> {
        let (effects, errors) = result;
        assert!(
            errors.is_empty(),
            "unexpected destroy plan errors: {errors:?}"
        );
        effects
    }

    #[tokio::test]
    async fn destroy_resolves_provider_config_from_upstream_state_before_construction() {
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        let received_role_arn = Arc::new(Mutex::new(None));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            })],
            &provider_configs,
        );
        let mut state = StateFile::new();
        state
            .upsert_resource(
                carina_state::ResourceState::try_applied("thing", "orphan", "test", "orphan")
                    .expect("fixture identifier is non-empty"),
            )
            .unwrap();
        let state_path = downstream.join(carina_state::LocalBackend::DEFAULT_STATE_FILE);
        std::fs::write(
            &state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
        let backend = crate::commands::local_backend_for_test(state_path);

        run_destroy_locked_with_ctx(
            validated_config,
            true,
            std::sync::Arc::from(backend),
            None,
            StateLock::disabled_for_test(),
            false,
            true,
            &downstream,
            &ProviderContext::default(),
            NonZeroUsize::new(1).unwrap(),
            ShutdownToken::running(),
        )
        .await
        .expect("destroy must construct the provider from resolved upstream attributes");

        assert_eq!(
            *received_role_arn.lock().unwrap(),
            Some(Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:role/upstream".into()
            )))
        );
    }

    #[tokio::test]
    async fn destroy_provider_resolution_ignores_unrelated_missing_upstream_state() {
        let (_temp, downstream, mut parsed) = provider_upstream_fixture();
        let unrelated = downstream.parent().unwrap().join("unrelated");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(
            unrelated.join("main.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .unwrap();
        parsed
            .upstream_states
            .push(carina_core::parser::UpstreamState {
                binding: "unrelated".to_string(),
                source: std::path::PathBuf::from("../unrelated"),
            });

        let backend =
            crate::commands::local_backend_for_test(downstream.join("missing-local-state.json"));
        run_destroy_locked(
            parsed,
            true,
            std::sync::Arc::from(backend),
            None,
            StateLock::disabled_for_test(),
            false,
            true,
            &downstream,
            &ProviderContext::default(),
            NonZeroUsize::new(1).unwrap(),
            ShutdownToken::running(),
        )
        .await
        .expect("destroy must not load an upstream unused by provider configuration");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn destroy_helper_releases_passed_lock_when_provider_load_hard_fails() {
        let project = tempfile::tempdir().unwrap();
        let source = format!(
            "file://{}",
            project.path().join("missing-provider.wasm").display()
        )
        .parse()
        .unwrap();
        let provider_config = carina_core::parser::ProviderConfig {
            name: "mock".to_string(),
            attributes: Default::default(),
            default_tags: Default::default(),
            source: Some(source),
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        };
        let parsed = carina_core::parser::InferredFile {
            providers: vec![provider_config.clone(), provider_config],
            ..Default::default()
        };
        let state_path = project.path().join("state.json");
        let lock_path = state_path.with_extension("lock");
        let backend: Arc<dyn StateBackend> =
            Arc::from(crate::commands::local_backend_for_test(state_path));
        let lock = backend.acquire_lock("destroy").await.unwrap();
        assert!(lock_path.exists(), "fixture must hold the lock");

        let error = run_destroy_locked(
            parsed,
            true,
            Arc::clone(&backend),
            None,
            StateLock::held(lock),
            false,
            true,
            project.path(),
            &ProviderContext::default(),
            NonZeroUsize::new(1).unwrap(),
            ShutdownToken::running(),
        )
        .await
        .expect_err("duplicate sourced providers must hard-fail loading");

        assert!(
            error.to_string().contains("declared more than once"),
            "unexpected loader error: {error}"
        );
        assert!(!lock_path.exists(), "load failure must release the lock");
    }

    #[tokio::test]
    async fn destroy_provider_resolution_reports_referenced_missing_upstream_state() {
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        std::fs::remove_file(
            downstream
                .parent()
                .unwrap()
                .join("upstream")
                .join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
        )
        .unwrap();
        let backend =
            crate::commands::local_backend_for_test(downstream.join("missing-local-state.json"));

        let error = match run_destroy_locked(
            parsed,
            true,
            std::sync::Arc::from(backend),
            None,
            StateLock::disabled_for_test(),
            false,
            true,
            &downstream,
            &ProviderContext::default(),
            NonZeroUsize::new(1).unwrap(),
            ShutdownToken::running(),
        )
        .await
        {
            Ok(_) => panic!("destroy must fail when provider-referenced state is missing"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("upstream_state 'mgmt'"), "{message}");
        assert!(message.contains("no state found"), "{message}");
        assert!(
            message.contains("required by provider 'test' attribute 'role_arn'"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn destroy_allows_default_tags_referencing_local_computed_attributes() {
        let (_temp, base_dir, parsed) = provider_default_tags_local_resource_fixture();
        let backend = crate::commands::local_backend_for_test(base_dir.join("state.json"));

        run_destroy_locked(
            parsed,
            true,
            std::sync::Arc::from(backend),
            None,
            StateLock::disabled_for_test(),
            false,
            true,
            &base_dir,
            &ProviderContext::default(),
            NonZeroUsize::new(1).unwrap(),
            ShutdownToken::running(),
        )
        .await
        .expect("destroy must defer provider default tags until resource normalization");
    }

    #[tokio::test]
    async fn destroy_ignores_missing_upstream_referenced_only_by_default_tags() {
        let (_temp, base_dir, parsed) = provider_default_tags_missing_upstream_fixture();
        let backend = crate::commands::local_backend_for_test(base_dir.join("state.json"));

        run_destroy_locked(
            parsed,
            true,
            std::sync::Arc::from(backend),
            None,
            StateLock::disabled_for_test(),
            false,
            true,
            &base_dir,
            &ProviderContext::default(),
            NonZeroUsize::new(1).unwrap(),
            ShutdownToken::running(),
        )
        .await
        .expect("destroy must not load upstream state used only by default tags");
    }

    #[test]
    fn destroy_parallelism_default_is_eight() {
        assert_eq!(crate::DEFAULT_PARALLELISM.get(), 8);
    }

    #[test]
    fn topological_delete_order_reports_cycles() {
        let deletion_deps = HashMap::from([(0, HashSet::from([1])), (1, HashSet::from([0]))]);
        let depths = compute_delete_depths(2, &deletion_deps);
        let names = vec!["a".to_string(), "b".to_string()];

        let err = topological_delete_order(2, &deletion_deps, &depths, &names).unwrap_err();

        assert_eq!(err, "Circular dependency detected: a -> b -> a");
    }

    #[test]
    fn topological_delete_order_uses_depth_tie_break_for_igw_nat_case() {
        let names = vec![
            "vpc".to_string(),
            "igw".to_string(),
            "subnet".to_string(),
            "eip".to_string(),
            "nat_gw".to_string(),
            "rt".to_string(),
            "route".to_string(),
        ];
        let deletion_deps = HashMap::from([
            (0, HashSet::from([1, 2, 5])),
            (1, HashSet::new()),
            (2, HashSet::from([4])),
            (3, HashSet::from([4])),
            (4, HashSet::from([6])),
            (5, HashSet::from([6])),
            (6, HashSet::new()),
        ]);
        let depths = compute_delete_depths(names.len(), &deletion_deps);

        let order = topological_delete_order(names.len(), &deletion_deps, &depths, &names)
            .expect("fixture is acyclic");

        let route_pos = order.iter().position(|idx| names[*idx] == "route").unwrap();
        let nat_pos = order
            .iter()
            .position(|idx| names[*idx] == "nat_gw")
            .unwrap();
        let igw_pos = order.iter().position(|idx| names[*idx] == "igw").unwrap();

        assert!(
            route_pos < igw_pos,
            "route must be destroyed before igw; order: {:?}",
            order.iter().map(|idx| &names[*idx]).collect::<Vec<_>>()
        );
        assert!(
            nat_pos < igw_pos,
            "nat_gw must be destroyed before igw; order: {:?}",
            order.iter().map(|idx| &names[*idx]).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn run_destroy_cancelled_after_partial_execution_persists_deletions_and_releases_lock() {
        let user_dirs = carina_core::TestUserDirs::new();
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["first", "second"])
            .cancel_after_successes(1);
        let token = fixture.cancel_token();
        let load_count = crate::wiring::track_provider_loads_for_test();

        let err = run_destroy(
            &user_dirs,
            fixture.config_path(),
            true,
            true,
            false,
            false,
            NonZeroUsize::new(1).unwrap(),
            fixture.provider_context(),
            token,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, AppError::Interrupted),
            "expected Interrupted, got {err:?}"
        );
        assert_eq!(load_count.count(), 1, "destroy must load providers once");
        let state = fixture.read_state().await;
        assert!(fixture.state_path().exists());
        assert!(
            state
                .find_resource("mock", "test.resource", "first")
                .is_none(),
            "first must be deleted from state"
        );
        assert!(
            state
                .find_resource("mock", "test.resource", "second")
                .is_some(),
            "second must remain in state"
        );
        assert!(!fixture.lock_path().exists(), "lock file must be released");
        assert_eq!(state.resources().len(), 1);
        assert!(state.serial >= 1, "state must be written");
        assert!(
            state.exports.is_empty(),
            "exports must be cleared on partial destroy"
        );
    }

    #[tokio::test]
    async fn destroy_reports_heartbeat_lock_loss_instead_of_user_interrupt() {
        let user_dirs = carina_core::TestUserDirs::new();
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["slow"])
            .with_delayed_delete("slow", Duration::from_millis(75));
        let loaded = load_configuration_with_config(
            fixture.config_path(),
            fixture.provider_context(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("fixture must load");
        let parsed = loaded.parsed;
        let base_dir = get_base_dir(fixture.config_path());
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, base_dir)
            .expect("mock provider must load");
        let validated_config = providers
            .validate_evaluated_config(
                parsed,
                base_dir,
                true,
                &loaded.inference_errors,
                &loaded.duplicate_declarations,
            )
            .expect("fixture must validate");
        let lock = LockInfo::for_test_with_timeout("destroy", Duration::from_millis(15));
        let backend = fixture.lock_loss_backend_handle();

        let error = run_destroy_locked_with_ctx(
            validated_config,
            true,
            backend.clone(),
            None,
            StateLock::held(lock),
            false,
            false,
            base_dir,
            fixture.provider_context(),
            NonZeroUsize::new(1).unwrap(),
            fixture.cancel_token(),
        )
        .await
        .expect_err("heartbeat lock loss must fail destroy");

        assert!(matches!(error, AppError::StateLockLost { .. }));
        assert!(error.to_string().contains("state may be behind the cloud"));
        assert_eq!(backend.locked_writes(), 0, "lost lock must not save state");
        assert_eq!(backend.releases(), 0, "lost lock must not be released");
    }

    #[tokio::test]
    async fn destroy_managed_refresh_cleanup_priority_abandons_in_flight_read_and_releases_lock() {
        let user_dirs = carina_core::TestUserDirs::new();
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["managed"])
            .with_blocked_read("managed")
            .with_refresh_drain_barrier();
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
            &user_dirs,
            fixture.config_path(),
            true,
            true,
            true,
            false,
            NonZeroUsize::new(1).unwrap(),
            fixture.provider_context(),
            shutdown,
        );
        tokio::pin!(destroy);
        tokio::select! {
            () = fixture.wait_for_blocked_read() => {}
            result = &mut destroy => {
                panic!("destroy returned before the managed refresh read blocked: {result:?}");
            }
        }
        assert!(fixture.lock_path().exists(), "fixture must hold the lock");
        fixture.request_graceful_shutdown();
        tokio::select! {
            () = fixture.wait_for_refresh_drain() => {}
            result = &mut destroy => {
                panic!("destroy returned before the managed refresh entered its drain wait: {result:?}");
            }
        }
        fixture.prioritize_cleanup_and_release_refresh_drain();
        let err = tokio::time::timeout(std::time::Duration::from_secs(1), destroy)
            .await
            .expect("destroy did not abandon the in-flight managed refresh after cleanup priority")
            .unwrap_err();

        assert!(matches!(err, AppError::Interrupted));
        assert_eq!(
            fixture.read_outcome(),
            "abandoned\n",
            "the in-flight managed refresh read must be dropped before producing a result"
        );
        let state = fixture.read_state().await;
        assert_eq!(state.serial, 0, "abandoned refresh must not save");
        assert!(
            state
                .find_resource("mock", "test.resource", "managed")
                .is_some(),
            "an abandoned refresh must preserve the persisted resource"
        );
        assert!(!fixture.lock_path().exists(), "lock must be released");
    }

    #[tokio::test]
    async fn destroy_orphan_refresh_cleanup_priority_abandons_in_flight_read_and_releases_lock() {
        let user_dirs = carina_core::TestUserDirs::new();
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_orphaned_resources(["orphan"])
            .with_blocked_read("orphan")
            .with_refresh_drain_barrier();
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
            &user_dirs,
            fixture.config_path(),
            true,
            true,
            true,
            false,
            NonZeroUsize::new(1).unwrap(),
            fixture.provider_context(),
            shutdown,
        );
        tokio::pin!(destroy);
        tokio::select! {
            () = fixture.wait_for_blocked_read() => {}
            result = &mut destroy => {
                panic!("destroy returned before the orphan refresh read blocked: {result:?}");
            }
        }
        assert!(fixture.lock_path().exists(), "fixture must hold the lock");
        fixture.request_graceful_shutdown();
        tokio::select! {
            () = fixture.wait_for_refresh_drain() => {}
            result = &mut destroy => {
                panic!("destroy returned before the orphan refresh entered its drain wait: {result:?}");
            }
        }
        fixture.prioritize_cleanup_and_release_refresh_drain();
        let err = tokio::time::timeout(std::time::Duration::from_secs(1), destroy)
            .await
            .expect("destroy did not abandon the in-flight orphan refresh after cleanup priority")
            .unwrap_err();

        assert!(matches!(err, AppError::Interrupted));
        assert_eq!(
            fixture.read_outcome(),
            "abandoned\n",
            "the in-flight orphan refresh read must be dropped before producing a result"
        );
        let state = fixture.read_state().await;
        assert_eq!(state.serial, 0, "abandoned refresh must not save");
        assert!(
            state
                .find_resource("mock", "test.resource", "orphan")
                .is_some(),
            "an abandoned refresh must preserve the persisted orphan"
        );
        assert!(!fixture.lock_path().exists(), "lock must be released");
    }

    #[tokio::test]
    async fn destroy_cleanup_priority_persists_completed_deletion_and_releases_lock() {
        let user_dirs = carina_core::TestUserDirs::new();
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["z_blocked", "a_completed"])
            .with_blocked_delete("z_blocked");
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
            &user_dirs,
            fixture.config_path(),
            true,
            true,
            false,
            false,
            NonZeroUsize::new(1).unwrap(),
            fixture.provider_context(),
            shutdown,
        );
        tokio::pin!(destroy);
        tokio::select! {
            () = fixture.wait_for_blocked_delete() => {}
            result = &mut destroy => panic!("destroy returned before the blocking delete started: {result:?}"),
        }
        fixture.prioritize_cleanup();
        let err = destroy.await.unwrap_err();

        assert!(matches!(err, AppError::Interrupted));
        let state = fixture.read_state().await;
        assert!(
            state
                .find_resource("mock", "test.resource", "a_completed")
                .is_none(),
            "the deletion folded before cleanup priority must be flushed"
        );
        assert!(
            state
                .find_resource("mock", "test.resource", "z_blocked")
                .is_some(),
            "the abandoned deletion must remain in state"
        );
        assert!(
            !fixture.lock_path().exists(),
            "cleanup must release the lock"
        );
    }

    #[tokio::test]
    async fn destroy_harvests_ready_deletion_before_cleanup_priority() {
        let user_dirs = carina_core::TestUserDirs::new();
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["ready"])
            .with_delete_result_barrier();
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
            &user_dirs,
            fixture.config_path(),
            true,
            true,
            false,
            false,
            NonZeroUsize::new(1).unwrap(),
            fixture.provider_context(),
            shutdown,
        );
        tokio::pin!(destroy);

        tokio::select! {
            () = fixture.wait_for_delete_result() => {}
            result = &mut destroy => {
                panic!("destroy returned before its provider result was released: {result:?}");
            }
        }

        fixture.release_delete_result_and_prioritize_cleanup();
        let err = destroy.await.unwrap_err();

        assert!(matches!(err, AppError::Interrupted));
        let state = fixture.read_state().await;
        assert!(
            state
                .find_resource("mock", "test.resource", "ready")
                .is_none(),
            "a deletion result that was already ready must be harvested before shutdown"
        );
        assert!(!fixture.lock_path().exists(), "lock must be released");
    }

    #[test]
    fn is_retryable_detects_dependency_violation() {
        let err = ProviderError::api_error(
            "DependencyViolation: Network vpc-xxx has some mapped public address(es)",
        );
        assert!(is_retryable_delete_error(&err));
    }

    #[test]
    fn is_retryable_detects_has_dependent_object() {
        let err = ProviderError::api_error("resource has a dependent object");
        assert!(is_retryable_delete_error(&err));
    }

    #[test]
    fn is_retryable_returns_false_for_generic_error() {
        let err = ProviderError::api_error("AccessDenied: not authorized");
        assert!(!is_retryable_delete_error(&err));
    }

    #[test]
    fn is_retryable_returns_false_for_timeout() {
        let err = ProviderError::timeout("DependencyViolation: something");
        assert!(!is_retryable_delete_error(&err));
    }

    #[test]
    fn build_destroy_delete_effects_includes_current_and_deposed_instances() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let resource =
            Resource::with_provider("awscc", "ec2.Vpc", "main", Some("current".to_string()))
                .with_binding("main");
        let current_states = HashMap::from([(
            resource.id.clone(),
            Current::from_state(
                State::existing(resource.id.clone(), HashMap::new()).with_identifier("vpc-new"),
            ),
        )]);
        let deposed_key = DeposedKey::new_unique();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.binding = Some("main".to_string());
        row.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key.clone(),
            "vpc-old".to_string(),
            Some("deposed".to_string()),
            HashMap::new(),
            BTreeSet::from(["subnet".to_string()]),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let effects = expect_valid_destroy_effects(build_destroy_delete_effects(
            &[&resource],
            &current_states,
            Some(&state_file),
            &HashSet::new(),
        ));

        assert_eq!(effects.len(), 2);
        assert!(matches!(
            &effects[0],
            Effect::Delete {
                identifier,
                generation: EffectGeneration::Current,
                ..
            } if identifier.as_str() == "vpc-new"
        ));
        assert!(matches!(
            &effects[1],
            Effect::Delete {
                id,
                identifier,
                generation: EffectGeneration::Deposed(key),
                dependencies,
                ..
            } if id.as_inner().provider_instance.as_deref() == Some("deposed")
                && identifier.as_str() == "vpc-old"
                && key == &deposed_key
                && dependencies == &HashSet::from(["subnet".to_string()])
        ));
    }

    #[test]
    fn destroy_delete_uses_identifier_carried_by_present_current() {
        let resource = Resource::with_provider("mock", "test.Widget", "current", None);
        let state = State::existing(resource.id.clone(), HashMap::new());
        let identifier = crate::fixture_applied_identifier("typed-provider-id");
        let current_states = HashMap::from([(
            resource.id.clone(),
            carina_core::resource::fixture_present_current(state, identifier),
        )]);

        let effects = expect_valid_destroy_effects(build_destroy_delete_effects(
            &[&resource],
            &current_states,
            None,
            &HashSet::new(),
        ));

        let [
            Effect::Delete {
                id,
                identifier,
                generation,
                directives,
                binding,
                dependencies,
                explicit_dependencies,
                blocked_by_updates,
            },
        ] = effects.as_slice()
        else {
            panic!("expected one Delete effect, got: {effects:?}");
        };
        assert_eq!(id.as_inner(), &resource.id);
        assert_eq!(identifier.as_str(), "typed-provider-id");
        assert_eq!(generation, &EffectGeneration::Current);
        assert_eq!(directives, &resource.directives);
        assert_eq!(binding, &resource.binding);
        assert!(dependencies.is_empty());
        assert!(explicit_dependencies.is_empty());
        assert!(blocked_by_updates.is_empty());
    }

    #[test]
    fn destroy_plan_rejects_identifier_less_row_loaded_from_state_json() {
        let resource = Resource::with_provider("mock", "test.Widget", "broken", None);
        let state_file = carina_state::check_and_migrate(
            &serde_json::json!({
                "version": carina_state::StateFile::CURRENT_VERSION,
                "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
                "serial": 1,
                "lineage": "destroy-loader-test",
                "carina_version": env!("CARGO_PKG_VERSION"),
                "resources": [{
                    "provider": "mock",
                    "resource_type": "test.Widget",
                    "identity": "broken",
                    "current": {
                        "kind": "missing_identifier",
                        "attributes": {},
                        "nested_write_only": {}
                    }
                }],
                "exports": {}
            })
            .to_string(),
        )
        .expect("malformed legacy row must remain loadable")
        .into_state();
        let loaded = state_file.build_state_for_resource(&resource.id);
        assert!(matches!(&loaded, Current::MissingIdentifier(_state)));
        let current_states = HashMap::from([(resource.id.clone(), loaded)]);

        let (effects, errors) =
            build_destroy_delete_effects(&[&resource], &current_states, None, &HashSet::new());
        assert!(effects.is_empty());
        let error = errors
            .first()
            .expect("destroy must reject a broken state row before provider dispatch");

        assert_eq!(
            error.kind,
            carina_core::plan::PlanErrorKind::MissingProviderIdentifier
        );
        let message = error.to_string();
        assert!(message.contains(&resource.id.to_string()), "{message}");
        assert!(message.contains("`removed` block"), "{message}");
        assert!(message.contains("`import` block"), "{message}");
    }

    #[test]
    fn destroy_rejects_in_flight_current() {
        use carina_core::provider::Sent;

        let first = Resource::with_provider("mock", "test.Widget", "first", None);
        let second = Resource::with_provider("mock", "test.Widget", "second", None);
        let first_pending = crate::fixture_in_flight_create(
            &first.id,
            "opaque-token",
            None,
            Sent::AllRequestAttributes,
        );
        let second_pending = crate::fixture_in_flight_create(
            &second.id,
            "other-opaque-token",
            None,
            Sent::AllRequestAttributes,
        );
        let current_states = HashMap::from([
            (first.id.clone(), Current::create_in_flight(first_pending)),
            (second.id.clone(), Current::create_in_flight(second_pending)),
        ]);

        let (effects, errors) = build_destroy_delete_effects(
            &[&first, &second],
            &current_states,
            None,
            &HashSet::new(),
        );

        assert!(effects.is_empty());
        assert_eq!(errors.len(), 2);
        for (error, expected_id) in errors.iter().zip([&first.id, &second.id]) {
            assert_eq!(&error.resource_id, expected_id);
            assert_eq!(
                error.kind,
                carina_core::plan::PlanErrorKind::DestroyCreateInFlight
            );
            let message = error.to_string();
            assert!(message.contains(&expected_id.to_string()), "{message}");
            assert!(message.contains("run apply first"), "{message}");
        }
    }

    #[test]
    fn orphan_destroy_uses_row_binding_and_depends_on_for_delete_ordering() {
        use carina_state::ResourceState;
        use std::collections::{BTreeSet, HashMap};

        let mut parent = ResourceState::try_applied("test.Parent", "parent", "mock", "parent-123")
            .expect("fixture identifier is non-empty");
        parent.binding = Some("parent".to_string());
        let mut child = ResourceState::try_applied("test.Child", "child", "mock", "child-123")
            .expect("fixture identifier is non-empty");
        child.binding = Some("child".to_string());
        child.directives.depends_on = vec!["parent".to_string()];
        child.dependency_bindings = BTreeSet::from(["network".to_string()]);

        let mut state_file = StateFile::new();
        state_file.upsert_resource(parent).unwrap();
        state_file.upsert_resource(child).unwrap();
        let orphan_entries = state_file.build_orphans(&HashSet::new());
        let mut resources = orphan_entries
            .iter()
            .map(|(id, _loaded)| {
                build_orphan_resource(
                    &state_file,
                    &ResolvedResourceId::try_new(id.clone())
                        .expect("test orphan id must be resolved"),
                )
            })
            .collect::<Vec<_>>();
        resources.sort_by_key(|resource| resource.binding.clone());
        let current_states = orphan_entries
            .into_iter()
            .map(|(id, loaded)| (id, loaded.current().clone()))
            .collect();
        let resource_refs = resources.iter().collect::<Vec<_>>();

        let effects = expect_valid_destroy_effects(build_destroy_delete_effects(
            &resource_refs,
            &current_states,
            Some(&state_file),
            &HashSet::new(),
        ));
        let child_delete = effects
            .iter()
            .find(|effect| effect.binding_name().as_deref() == Some("child"))
            .expect("child orphan must produce a Delete");
        assert!(matches!(
            child_delete,
            Effect::Delete {
                dependencies,
                explicit_dependencies,
                ..
            } if dependencies == &HashSet::from(["network".to_string(), "parent".to_string()])
                && explicit_dependencies == &HashSet::from(["parent".to_string()])
        ));

        let analysis = build_effect_dependency_analysis(
            &effects,
            &HashMap::<ResourceId, UnresolvedResource>::new(),
            &[],
            ScheduleInputs::Destroy { aliases: &[] },
        )
        .expect("destroy dependency analysis must resolve");
        let deletion_deps = transitive_delete_deps(&analysis, effects.len());
        let depths = compute_delete_depths(effects.len(), &deletion_deps);
        let names = effects
            .iter()
            .map(|effect| effect.binding_name().unwrap())
            .collect::<Vec<_>>();
        let order = topological_delete_order(effects.len(), &deletion_deps, &depths, &names)
            .expect("orphan dependency graph must be acyclic");
        assert_eq!(
            order
                .into_iter()
                .map(|index| names[index].as_str())
                .collect::<Vec<_>>(),
            vec!["child", "parent"]
        );
    }

    #[test]
    fn build_destroy_delete_effects_skips_deposed_generations_for_protected_rows() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let resource =
            Resource::with_provider("awscc", "s3.Bucket", "state", None).with_binding("state");
        let mut row = ResourceState::new("s3.Bucket", "state", "awscc");
        row.binding = Some("state".to_string());
        row.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "old-state-bucket".to_string(),
            None,
            HashMap::new(),
            BTreeSet::new(),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");
        let protected = HashSet::from([state_row_key_from_id(&ResolvedResourceId::new(
            resource.id.clone(),
        ))]);

        let effects = expect_valid_destroy_effects(build_destroy_delete_effects(
            &[],
            &HashMap::new(),
            Some(&state_file),
            &protected,
        ));

        assert!(
            effects.is_empty(),
            "a protected current row protects its deposed generations too"
        );
    }

    #[test]
    fn build_destroy_delete_effects_keeps_deposed_generations_for_prevent_destroy_rows() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.binding = Some("main".to_string());
        row.directives.prevent_destroy = true;
        let deposed_key = DeposedKey::new_unique();
        row.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key.clone(),
            "vpc-old".to_string(),
            None,
            HashMap::new(),
            BTreeSet::new(),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let effects = expect_valid_destroy_effects(build_destroy_delete_effects(
            &[],
            &HashMap::new(),
            Some(&state_file),
            &HashSet::new(),
        ));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::Delete {
                identifier,
                generation: EffectGeneration::Deposed(key),
                ..
            } if identifier.as_str() == "vpc-old" && key == &deposed_key
        ));
    }

    #[test]
    fn deposed_only_destroy_work_prevents_all_protected_short_circuit() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "vpc-old".to_string(),
            None,
            HashMap::new(),
            BTreeSet::new(),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let effects = expect_valid_destroy_effects(build_destroy_delete_effects(
            &[],
            &HashMap::new(),
            Some(&state_file),
            &HashSet::new(),
        ));

        assert_eq!(effects.len(), 1);
        assert!(
            !should_skip_destroy_execution(effects.len()),
            "deposed-only destroy work must still execute"
        );
    }

    #[test]
    fn destroy_summary_count_uses_actual_delete_effects() {
        let counts = destroy_plan_counts(2, 1, 0);

        assert_eq!(
            counts,
            DestroyPlanCounts {
                to_destroy: 2,
                guarded: 1
            }
        );
    }

    #[test]
    fn apply_destroy_to_state_removes_resources_and_clears_exports() {
        // Regression test for #1983: destroy must clear stale exports because
        // they reference attributes of resources that no longer exist.
        use carina_state::ResourceState;

        let mut state = carina_state::StateFile::new();
        state
            .upsert_resource(ResourceState::new("ec2.Vpc", "main", "awscc"))
            .expect("test state setup must be valid");
        state.insert_export_json_for_test("vpc_id", serde_json::json!("vpc-12345"));

        let destroyed = vec![DestroyedInstance {
            id: ResolvedResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None),
            generation: EffectGeneration::Current,
        }];
        apply_destroy_to_state(&mut state, &destroyed, DestroyStateWrite::Final);

        assert!(state.resources().is_empty(), "resource should be removed");
        assert!(state.exports.is_empty(), "exports should be cleared");
    }

    #[test]
    fn destroy_checkpoint_keeps_exports_until_the_final_save() {
        use carina_state::ResourceState;

        let mut state = carina_state::StateFile::new();
        for name in ["first", "second"] {
            state
                .upsert_resource(ResourceState::new("ec2.Vpc", name, "awscc"))
                .expect("test state setup must be valid");
        }
        state.insert_export_json_for_test("vpc_id", serde_json::json!("vpc-12345"));
        let destroyed = [DestroyedInstance {
            id: ResolvedResourceId::with_provider_identity("awscc", "ec2.Vpc", "first", None),
            generation: EffectGeneration::Current,
        }];

        apply_destroy_to_state(&mut state, &destroyed, DestroyStateWrite::Checkpoint);

        assert!(
            state.find_resource("awscc", "ec2.Vpc", "first").is_none(),
            "the completed delete must be durable"
        );
        assert!(
            state.find_resource("awscc", "ec2.Vpc", "second").is_some(),
            "unfinished deletes must retain their rows"
        );
        assert_eq!(
            state
                .exports
                .get("vpc_id")
                .and_then(|value| serde_json::to_value(value).ok()),
            Some(serde_json::json!("vpc-12345")),
            "mid-run checkpoints must not wipe exports"
        );
    }

    #[test]
    fn apply_destroy_to_state_preserves_deposed_generations() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let mut state = carina_state::StateFile::new();
        let mut resource = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-new")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
        resource.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "vpc-old".to_string(),
            None,
            HashMap::from([("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))]),
            BTreeSet::from(["network".to_string()]),
        ));
        state
            .upsert_resource(resource)
            .expect("test state setup must be valid");
        state.insert_export_json_for_test("vpc_id", serde_json::json!("vpc-new"));

        let destroyed = vec![DestroyedInstance {
            id: ResolvedResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None),
            generation: EffectGeneration::Current,
        }];
        apply_destroy_to_state(&mut state, &destroyed, DestroyStateWrite::Final);

        let retained = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("row with deposed generations should be retained");
        assert_eq!(retained.identifier(), None);
        assert!(retained.persisted_attributes().is_none());
        assert_eq!(retained.deposed.len(), 1);
        assert_eq!(retained.deposed[0].identifier, "vpc-old");
        assert!(state.exports.is_empty(), "exports should be cleared");
    }

    #[test]
    fn apply_destroy_to_state_removes_only_successful_deposed_generation() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let removed_key = DeposedKey::new_unique();
        let retained_key = DeposedKey::new_unique();
        let mut state = carina_state::StateFile::new();
        let mut resource = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-new")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
        resource.deposed.push(DeposedInstance::from_persisted_json(
            removed_key.clone(),
            "vpc-old".to_string(),
            None,
            HashMap::from([("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))]),
            BTreeSet::from(["network".to_string()]),
        ));
        resource.deposed.push(DeposedInstance::from_persisted_json(
            retained_key.clone(),
            "vpc-older".to_string(),
            None,
            HashMap::from([("cidr_block".to_string(), serde_json::json!("10.2.0.0/16"))]),
            BTreeSet::from(["network".to_string()]),
        ));
        state
            .upsert_resource(resource)
            .expect("test state setup must be valid");

        let destroyed = vec![DestroyedInstance {
            id: ResolvedResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None),
            generation: EffectGeneration::Deposed(removed_key),
        }];
        apply_destroy_to_state(&mut state, &destroyed, DestroyStateWrite::Final);

        let retained = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain after deposed-only destroy result");
        assert_eq!(
            retained.identifier().map(ToString::to_string).as_deref(),
            Some("vpc-new")
        );
        assert_eq!(retained.deposed.len(), 1);
        assert_eq!(retained.deposed[0].key, retained_key);
        assert_eq!(retained.deposed[0].identifier, "vpc-older");
    }

    #[test]
    fn apply_destroy_to_state_removes_row_after_current_and_deposed_successes() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let deposed_key = DeposedKey::new_unique();
        let mut state = carina_state::StateFile::new();
        let mut resource = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-new")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
        resource.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key.clone(),
            "vpc-old".to_string(),
            None,
            HashMap::from([("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))]),
            BTreeSet::from(["network".to_string()]),
        ));
        state
            .upsert_resource(resource)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
        let destroyed = vec![
            DestroyedInstance {
                id: ResolvedResourceId::new(id.clone()),
                generation: EffectGeneration::Current,
            },
            DestroyedInstance {
                id: ResolvedResourceId::new(id),
                generation: EffectGeneration::Deposed(deposed_key),
            },
        ];
        apply_destroy_to_state(&mut state, &destroyed, DestroyStateWrite::Final);

        assert!(
            state.find_resource("awscc", "ec2.Vpc", "main").is_none(),
            "row should drop only after both current and deposed generations are removed"
        );
    }
}
