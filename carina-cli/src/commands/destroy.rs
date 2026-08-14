use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::get_resource_dependencies;
use carina_core::effect::deps::{
    DependencyAnalysis, DestroyWaitAlias, ScheduleInputs, UnresolvedResource,
    build_effect_dependency_analysis,
};
use carina_core::effect::{Effect, EffectGeneration};
use carina_core::parser::WaitBinding;
use carina_core::plan::Plan;
use carina_core::provider::Provider;
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};
#[cfg(test)]
use carina_core::shutdown::testing::TestShutdownTrigger;
use carina_core::shutdown::{
    CleanupInterrupted, LoopShutdownPhase, LoopStep, ShutdownPhase, ShutdownToken,
};
use carina_state::{LockInfo, StateBackend, StateFile};

use carina_core::parser::ProviderContext;

use super::{DriftCommand, verify_for_mutation};
use crate::DetailLevel;
use crate::commands::plan::collect_delete_attributes;
use crate::commands::shared::finalize::{
    StatePersistence, finalize_after_execute, release_lock_after_execute,
};
use crate::commands::shared::progress::{
    RefreshProgress, format_duration, refresh_multi_progress, spinner_style,
};
use crate::commands::shared::retry::{WaitResult, is_retryable_delete_error, wait_for_deletion};
use crate::commands::shared::state_writeback::{
    DestroyedInstance, apply_destroy_to_state, apply_name_overrides, build_orphan_resource,
};
use crate::commands::state::map_lock_error;
use crate::cursor::CursorReveal;
use crate::display::{format_destroy_plan_with_delete_instances, format_effect};
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, get_provider_with_ctx, read_with_retry,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
};

#[allow(clippy::too_many_arguments)]
pub async fn run_destroy(
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
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    crate::commands::validate_and_resolve_with_config(
        &mut parsed,
        base_dir,
        true,
        &inference_errors,
        &duplicate_declarations,
    )?;

    let verified_backend =
        verify_for_mutation(base_dir, parsed.backend.as_ref(), DriftCommand::Destroy)?;

    // Don't exit early when resources are empty -- orphaned resources in the
    // state file may still need to be destroyed.

    // Check for backend configuration - use local backend by default
    let backend: Box<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?;

    // Get the state bucket name for protection check (S3 backend only)
    let protected_bucket = verified_backend
        .string_attribute("bucket")
        .map(str::to_owned);

    // Acquire lock (unless --lock=false)
    let lock_info: Option<LockInfo> = if lock {
        println!("{}", "Acquiring state lock...".cyan());
        let li = backend
            .acquire_lock("destroy")
            .await
            .map_err(map_lock_error)?;
        println!("  {} Lock acquired", "✓".green());
        Some(li)
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow()
                .bold()
        );
        None
    };

    let op_result = run_destroy_locked(
        &mut parsed,
        auto_approve,
        backend.as_ref(),
        protected_bucket,
        lock_info.as_ref(),
        refresh,
        force,
        base_dir,
        parallelism,
        cancel.clone(),
    )
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = release_lock_after_execute(backend.as_ref(), li, &cancel).await;

        if release_result.is_ok()
            && (op_result.is_ok() || matches!(op_result, Err(AppError::Interrupted)))
        {
            println!("  {} Lock released", "✓".green());
        }

        op_result?;
        release_result?;
    } else {
        op_result?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_destroy_locked(
    parsed: &mut carina_core::parser::InferredFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    protected_bucket: Option<String>,
    lock: Option<&LockInfo>,
    refresh: bool,
    force: bool,
    base_dir: &std::path::Path,
    parallelism: NonZeroUsize,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir)?;
    let ctx = WiringContext::new(factories);

    // Read current state from backend. carina#3315: persist any older-schema
    // migration under the destroy lock before any short-circuit
    // (e.g. "No resources to destroy.") returns — see
    // `apply::load_state_persist_if_migrated`.
    let mut state_file =
        crate::commands::apply::load_state_persist_if_migrated(backend, lock).await?;

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    let state_block_claims = crate::wiring::resolve_state_block_claims(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        ctx.schemas(),
    );
    if let Some(sf) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.identity.clone())
                    .collect()
            },
            &state_block_claims,
        );
    }
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(
            &ctx,
            &mut parsed.resources,
            sf,
            &state_block_claims,
        )?;
    }
    // destroy operates on the existing state-only resource set and does not run the
    // differ. The state-side name_overrides are the only thing destroy needs from
    // this surface; the full resolver -> override -> bindings rebuild ->
    // second-pass resolver sequence is unnecessary here. Sub-PR B leaves this
    // call site as-is for that reason. See
    // notes/specs/2026-06-28-issue-3625-cbd-decompose-clean-design.md Phase 5 T5.8.
    apply_name_overrides(&mut parsed.resources, &state_file);

    // Collect all resources (managed + orphans) before sorting.
    // We use the unsorted list for state reads, then sort once at the end.
    let mut all_resources: Vec<Resource> = parsed.resources.clone(); // allow: direct — plan-time reconciliation

    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached state (--refresh=false). Plan may not reflect actual infrastructure.".yellow()
        );
    }

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(&ctx, parsed, base_dir).await?;

    // Build current states -- either from provider (refresh=true) or from state file
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();

    if refresh {
        RefreshProgress::start_header();
        let multi = refresh_multi_progress();

        // Read states for managed resources concurrently using identifier
        // from state. carina#3181: `all_resources` is sourced from
        // `parsed.resources`, which is managed-only — data sources and
        // composition resources are not destroyed and never enter this list.
        let resources: Vec<&Resource> = all_resources.iter().collect();
        let provider_ref = &provider;
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
                let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                let identifier = state_file
                    .as_ref()
                    .and_then(|sf| sf.get_identifier_for_resource(resource));
                refresh_in_flight.push(async move {
                    let state = read_with_retry(provider_ref, &resource.id, identifier.as_deref())
                        .await
                        .map_err(AppError::Provider)?;
                    progress.finish();
                    Ok((resource.id.clone(), state))
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
            let result: Result<(ResourceId, State), AppError> = match next {
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
            let (id, state) = result?;
            current_states.insert(id, state);
        }
        drop(refresh_in_flight);
        drop(resource_iter);
        if refresh_cancelled {
            return Err(AppError::Interrupted);
        }

        // Include orphaned resources (in state but not in .crn).
        // Refresh each orphan concurrently via provider.read() to verify it still exists.
        if let Some(sf) = state_file.as_ref() {
            let desired_ids: HashSet<ResourceId> =
                all_resources.iter().map(|r| r.id.clone()).collect();
            let orphan_states: Vec<(ResourceId, State)> =
                sf.build_orphan_states(&desired_ids).into_iter().collect();
            let mut refresh_cancelled = false;
            let mut orphan_iter = orphan_states.into_iter();
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
                    let Some((id, state)) = orphan_iter.next() else {
                        break;
                    };
                    let progress = RefreshProgress::begin_multi(&multi, &id);
                    orphan_in_flight.push(async move {
                        let refreshed =
                            read_with_retry(provider_ref, &id, state.identifier.as_deref())
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
                let result: Result<(ResourceId, State), AppError> = match next {
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
                let (id, refreshed) = result?;
                if refreshed.exists {
                    current_states.insert(id.clone(), refreshed);
                    let orphan_resource = build_orphan_resource(sf, &id);
                    all_resources.push(orphan_resource);
                }
            }
            if refresh_cancelled {
                return Err(AppError::Interrupted);
            }
        }
    } else if let Some(sf) = state_file.as_ref() {
        // --refresh=false: build states from state file without AWS calls.
        // carina#3181: `all_resources` is managed-only.
        for resource in &all_resources {
            let state = sf.build_state_for_resource(&resource.id);
            current_states.insert(resource.id.clone(), state);
        }

        // Include orphaned resources (in state but not in .crn)
        let desired_ids: HashSet<ResourceId> = all_resources.iter().map(|r| r.id.clone()).collect();
        for (id, state) in sf.build_orphan_states(&desired_ids) {
            current_states.insert(id.clone(), state);
            let orphan_resource = build_orphan_resource(sf, &id);
            all_resources.push(orphan_resource);
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
            if !current_states.get(&r.id).map(|s| s.exists).unwrap_or(false) {
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
            .as_deref()
            .unwrap_or_else(|| left.id.identity_or_empty());
        let right_key = right
            .binding
            .as_deref()
            .unwrap_or_else(|| right.id.identity_or_empty());
        left_key.cmp(right_key)
    });

    // Backend-bucket protection shields the whole state row, including deposed
    // generations, because deleting any generation could destroy the state
    // bucket itself. `prevent_destroy` is intentionally not included here: it
    // guards the current instance, while a deposed generation already came from
    // a replacement path that approved deleting the old instance.
    let protected_row_keys: HashSet<StateRowKey> = protected_resources
        .iter()
        .map(|resource| state_row_key_from_id(&resource.id))
        .collect();
    let delete_effects = build_destroy_delete_effects(
        &resources_to_destroy,
        &current_states,
        state_file.as_ref(),
        &protected_row_keys,
    );

    let wait_aliases = build_destroy_wait_aliases(&parsed.wait_bindings, &delete_effects);
    let dependency_analysis = build_effect_dependency_analysis(
        &delete_effects,
        &HashMap::<ResourceId, UnresolvedResource>::new(),
        &[],
        ScheduleInputs::Destroy {
            aliases: &wait_aliases,
        },
    );
    let delete_count = delete_effects.len();
    let deletion_deps = transitive_delete_deps(&dependency_analysis, delete_count);
    let delete_depths = compute_delete_depths(delete_count, &deletion_deps);
    let resource_names: Vec<String> = delete_effects
        .iter()
        .map(|effect| {
            effect
                .binding_name()
                .unwrap_or_else(|| effect.resource_id().identity_or_empty().to_string())
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
        return Ok(());
    }

    // Build a Plan from the delete effects for tree display
    let mut destroy_plan = Plan::new();
    for idx in &display_order {
        destroy_plan.add(delete_effects[*idx].clone());
    }

    let delete_attributes =
        collect_delete_attributes(&destroy_plan, &current_states, state_file.as_ref());

    // Display destroy plan as a dependency tree
    print!(
        "{}",
        format_destroy_plan_with_delete_instances(
            &destroy_plan,
            DetailLevel::Full,
            &delete_attributes
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
        for resource in &prevent_destroy_resources {
            println!("  {} {}", "✗".red().bold(), resource.id);
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
        return Ok(());
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
            return Ok(());
        }
        println!();
    }

    println!("{}", "Destroying resources...".red().bold());
    println!();

    // Set up multi-progress for concurrent spinners
    let multi = MultiProgress::new();
    if !std::io::stdout().is_terminal() {
        multi.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    }

    // Map from resource index to its spinner (populated lazily on dispatch)
    let mut spinners: HashMap<usize, ProgressBar> = HashMap::new();

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut destroyed_instances: Vec<DestroyedInstance> = Vec::new();
    let mut failed_indices: HashSet<usize> = HashSet::new();
    let mut cancelled = false;
    // timed_out_resources: delete index -> (ResourceId, identifier, generation)
    let mut timed_out_resources: HashMap<usize, (ResourceId, String, EffectGeneration)> =
        HashMap::new();

    let destroy_total = delete_effects.len();
    let completed_counter = AtomicUsize::new(0);

    // Pre-compute binding and effect for each resource by index
    let resource_info: Vec<(String, Effect)> = delete_effects
        .iter()
        .map(|effect| {
            let binding = effect.binding_name().unwrap_or_else(|| {
                let id = effect.resource_id();
                format!("{}:{}", id.resource_type, id.identity_or_empty())
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

                // Check if any dependent has actually failed (non-timeout)
                if let Some(failed_dep_idx) = delete_dependency_in_set(
                    idx,
                    &dependency_analysis,
                    delete_count,
                    &failed_indices,
                ) {
                    let c = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let counter = format!("{}/{}", c, destroy_total).dimmed();
                    let failed_dep = &resource_info[failed_dep_idx].0;
                    let msg = format!(
                        "{} {} - skipped (dependent {} failed) {}",
                        "⊘".yellow(),
                        format_effect(effect),
                        failed_dep,
                        counter
                    );
                    if let Some(pb) = spinners.remove(&idx) {
                        pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                        pb.finish_with_message(msg);
                    } else {
                        eprintln!("  {}", msg);
                    }
                    skip_count += 1;
                    failed_indices.insert(idx);
                    completed_indices.insert(idx);
                    continue;
                }

                // Check if any dependent timed out -- wait for it to complete
                let timed_out_deps = delete_dependencies_in_set(
                    idx,
                    &dependency_analysis,
                    delete_count,
                    timed_out_resources.keys().copied().collect(),
                );

                let mut wait_failed = false;
                for dep_idx in &timed_out_deps {
                    if let Some((dep_id, dep_identifier, dep_generation)) =
                        timed_out_resources.remove(dep_idx)
                    {
                        multi
                            .println(format!(
                                "  {} Waiting for {} to be deleted...",
                                "⏳".yellow(),
                                dep_id
                            ))
                            .ok();

                        let wait_result = wait_for_deletion(
                            &cleanup_loop,
                            &provider,
                            &dep_id,
                            &dep_identifier,
                            180,
                            std::time::Duration::from_secs(10),
                        )
                        .await;
                        match wait_result {
                            WaitResult::Abandoned => {
                                cancelled = true;
                                break 'destroy;
                            }
                            WaitResult::Deleted => {
                                multi
                                    .println(format!(
                                        "  {} Delete {} (completed after extended wait)",
                                        "✓".green(),
                                        dep_id
                                    ))
                                    .ok();
                                destroyed_instances.push(DestroyedInstance {
                                    id: dep_id.clone(),
                                    generation: dep_generation,
                                });
                                success_count += 1;
                            }
                            WaitResult::ReadError(msg) => {
                                multi
                                    .println(format!("  {} Delete {}", "✗".red(), dep_id))
                                    .ok();
                                multi
                                    .println(format!(
                                        "      {} {}",
                                        "→".red(),
                                        format!("read error during wait: {}", msg).red()
                                    ))
                                    .ok();
                                failed_indices.insert(*dep_idx);
                                failure_count += 1;
                                wait_failed = true;
                            }
                            WaitResult::TimedOut => {
                                multi
                                    .println(format!("  {} Delete {}", "✗".red(), dep_id))
                                    .ok();
                                multi
                                    .println(format!(
                                        "      {} {}",
                                        "→".red(),
                                        "still exists after extended wait".red()
                                    ))
                                    .ok();
                                failed_indices.insert(*dep_idx);
                                failure_count += 1;
                                wait_failed = true;
                            }
                        }
                    }
                }

                if wait_failed {
                    let c = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let counter = format!("{}/{}", c, destroy_total).dimmed();
                    let msg = format!(
                        "{} {} - skipped (dependent deletion did not complete) {}",
                        "⊘".yellow(),
                        format_effect(effect),
                        counter
                    );
                    if let Some(pb) = spinners.remove(&idx) {
                        pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                        pb.finish_with_message(msg);
                    } else {
                        eprintln!("  {}", msg);
                    }
                    skip_count += 1;
                    failed_indices.insert(idx);
                    completed_indices.insert(idx);
                    continue;
                }

                // Create a spinner for the in-flight deletion
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style());
                pb.set_message(format_effect(effect));
                pb.enable_steady_tick(Duration::from_millis(80));
                spinners.insert(idx, pb);

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
                let resource_id = id.clone().into_inner();
                let identifier = identifier.clone();
                let generation = generation.clone();
                let directives = directives.clone();

                let provider_ref = &provider;
                let result_shutdown = &cancel;
                in_flight.push(async move {
                    let started = Instant::now();
                    let delete_result = provider_ref
                        .delete(
                            &resource_id,
                            &identifier,
                            carina_core::provider::DeleteRequest {
                                directives: directives.clone(),
                            },
                        )
                        .await;
                    observe_destroy_result_ready_for_tests(result_shutdown).await;
                    (
                        idx,
                        resource_id,
                        identifier,
                        generation,
                        started,
                        delete_result,
                    )
                });
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
                    let c = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let counter = format!("{}/{}", c, destroy_total).dimmed();
                    let msg = format!(
                        "{} {} - retries exhausted (no progress possible) {}",
                        "✗".red(),
                        format_effect(effect),
                        counter
                    );
                    if let Some(pb) = spinners.remove(&idx) {
                        pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                        pb.finish_with_message(msg);
                    } else {
                        eprintln!("  {}", msg);
                    }
                    failed_indices.insert(idx);
                    dispatched.insert(idx);
                    completed_indices.insert(idx);
                    failure_count += 1;
                }
                break;
            }
            // Non-retry cycle: skip remaining
            for &idx in &remaining {
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
        let (finished_idx, resource_id, identifier, generation, started, delete_result) = match next
        {
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

        let c = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let counter = format!("{}/{}", c, destroy_total).dimmed();
        let effect = &resource_info[finished_idx].1;

        // Helper to finish the spinner for the completed effect.
        // Always prints via eprintln when stdout is not a terminal,
        // because indicatif suppresses spinner output in non-terminal contexts.
        let is_terminal = std::io::stdout().is_terminal();
        let finish_spinner =
            |spinners: &mut HashMap<usize, ProgressBar>, idx: usize, msg: String| {
                if let Some(pb) = spinners.remove(&idx) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg.clone());
                    if !is_terminal {
                        eprintln!("  {}", msg);
                    }
                } else {
                    eprintln!("  {}", msg);
                }
            };

        match delete_result {
            Ok(()) => {
                let timing = format!("took {}", format_duration(started.elapsed())).dimmed();
                let msg = format!(
                    "{} {} {} {}",
                    "✓".green(),
                    format_effect(effect),
                    timing,
                    counter
                );
                finish_spinner(&mut spinners, finished_idx, msg);
                success_count += 1;
                observe_destroy_success_for_tests(success_count, &resource_id, &cancel);
                destroyed_instances.push(DestroyedInstance {
                    id: resource_id,
                    generation,
                });
            }
            Err(carina_core::provider::ProviderError::Timeout(_)) => {
                let msg = format!(
                    "{} {} - Operation timed out, waiting for completion...",
                    "⏳".yellow(),
                    format_effect(effect)
                );
                finish_spinner(&mut spinners, finished_idx, msg);
                timed_out_resources.insert(finished_idx, (resource_id, identifier, generation));
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
                    completed_counter.fetch_sub(1, Ordering::Relaxed);
                    let retry_num = retry_counts[&finished_idx];
                    let msg = format!(
                        "{} {} - dependency violation, will retry ({}/{})",
                        "↻".yellow(),
                        format_effect(effect),
                        retry_num,
                        max_retries
                    );
                    finish_spinner(&mut spinners, finished_idx, msg);
                } else {
                    let timing = format!("took {}", format_duration(started.elapsed())).dimmed();
                    let msg = format!(
                        "{} {} {} {}\n      {} {}",
                        "✗".red(),
                        format_effect(effect),
                        timing,
                        counter,
                        "→".red(),
                        e.to_string().red()
                    );
                    finish_spinner(&mut spinners, finished_idx, msg);
                    failure_count += 1;
                    failed_indices.insert(finished_idx);
                }
            }
        }
    }
    drop(in_flight);

    // Handle any remaining timed-out resources that no parent waited on.
    if !cancelled {
        for (dep_idx, (dep_id, dep_identifier, dep_generation)) in &timed_out_resources {
            let cleanup_wait = match cleanup_loop.step() {
                LoopStep::Continue(wait) => wait,
                LoopStep::Abandon => {
                    cancelled = true;
                    break;
                }
            };
            match cleanup_wait.phase() {
                LoopShutdownPhase::Running => {}
                LoopShutdownPhase::Graceful => {
                    cancelled = true;
                    break;
                }
            }

            eprintln!(
                "  {} Waiting for {} to be deleted...",
                "⏳".yellow(),
                dep_id
            );

            let outcome = wait_for_deletion(
                &cleanup_loop,
                &provider,
                dep_id,
                dep_identifier,
                180,
                std::time::Duration::from_secs(10),
            );
            tokio::pin!(outcome);
            let outcome = tokio::select! {
                biased;
                outcome = &mut outcome => outcome,
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
            };

            match outcome {
                WaitResult::Abandoned => {
                    cancelled = true;
                    break;
                }
                WaitResult::Deleted => {
                    eprintln!(
                        "  {} Delete {} (completed after extended wait)",
                        "✓".green(),
                        dep_id
                    );
                    destroyed_instances.push(DestroyedInstance {
                        id: dep_id.clone(),
                        generation: dep_generation.clone(),
                    });
                    success_count += 1;
                }
                WaitResult::ReadError(msg) => {
                    eprintln!("  {} Delete {}", "✗".red(), dep_id);
                    eprintln!(
                        "      {} {}",
                        "→".red(),
                        format!("read error during wait: {}", msg).red()
                    );
                    failed_indices.insert(*dep_idx);
                    failure_count += 1;
                }
                WaitResult::TimedOut => {
                    eprintln!("  {} Delete {}", "✗".red(), dep_id);
                    eprintln!(
                        "      {} {}",
                        "→".red(),
                        "still exists after extended wait".red()
                    );
                    failed_indices.insert(*dep_idx);
                    failure_count += 1;
                }
            }
        }
    }

    if cancelled && destroyed_instances.is_empty() {
        return Err(AppError::Interrupted);
    }

    println!();
    println!("{}", "Saving state...".cyan());

    finalize_after_execute(
        |persistence| {
            finalize_destroy(
                FinalizeDestroyInput {
                    backend,
                    lock,
                    state_file,
                    destroyed_instances: &destroyed_instances,
                },
                persistence,
            )
        },
        cancelled,
        &cancel,
    )
    .await?;

    println!();
    if failure_count == 0 && skip_count == 0 {
        println!(
            "{}",
            format!("Destroy complete! {} resources destroyed.", success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "Destroy failed. {} succeeded, {} failed, {} skipped.",
            success_count, failure_count, skip_count
        )))
    }
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
                    .unwrap_or_else(|| id.identity_or_empty().to_string()),
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
    current_states: &HashMap<ResourceId, State>,
    state_file: Option<&StateFile>,
    protected_row_keys: &HashSet<StateRowKey>,
) -> Vec<Effect> {
    let mut delete_effects = Vec::with_capacity(resources_to_destroy.len());
    for resource in resources_to_destroy {
        let identifier = current_states
            .get(&resource.id)
            .and_then(|s| s.identifier.clone())
            .unwrap_or_default();
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
                &row.identity,
            )) {
                continue;
            }
            delete_effects.extend(crate::wiring::deposed_delete_effects_for_row(row));
        }
    }
    delete_effects
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

fn state_row_key_from_id(id: &ResourceId) -> StateRowKey {
    state_row_key_from_parts(&id.provider, &id.resource_type, id.identity_or_empty())
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

fn delete_dependencies_in_set(
    idx: usize,
    analysis: &DependencyAnalysis,
    delete_count: usize,
    candidates: HashSet<usize>,
) -> Vec<usize> {
    let mut deps: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| {
            *candidate < delete_count && graph_reaches_delete_idx(*candidate, idx, analysis)
        })
        .collect();
    deps.sort_unstable();
    deps
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
    pub lock: Option<&'a LockInfo>,
    pub state_file: Option<StateFile>,
    pub destroyed_instances: &'a [DestroyedInstance],
}

pub(crate) async fn finalize_destroy(
    input: FinalizeDestroyInput<'_>,
    persistence: StatePersistence,
) -> Result<(), AppError> {
    let mut state = input.state_file.unwrap_or_default();

    // NOTE: apply_destroy_to_state currently clears state.exports unconditionally.
    // On a cancelled partial destroy, exports for resources that survived are
    // also wiped. This is pre-existing behavior; downstream consumers should
    // re-derive exports from a fresh plan. See carina-cli/src/commands/shared/state_writeback.rs.
    apply_destroy_to_state(&mut state, input.destroyed_instances);

    if let Some(lock) = input.lock {
        crate::commands::apply::save_state_locked_after_execute(
            input.backend,
            lock,
            &mut state,
            persistence,
        )
        .await?;
    } else {
        crate::commands::apply::save_state_unlocked_after_execute(
            input.backend,
            &mut state,
            persistence,
        )
        .await?;
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult};
    use carina_core::resource::DataSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[path = "cancellation_fixture.rs"]
    mod cancellation_fixture;

    use cancellation_fixture::DestroyCancellationFixture;

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
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["first", "second"])
            .cancel_after_successes(1);
        let token = fixture.cancel_token();

        let err = run_destroy(
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
        let state = fixture.read_state().await;
        assert!(fixture.backend().state_path().exists());
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
    async fn destroy_managed_refresh_cleanup_priority_abandons_in_flight_read_and_releases_lock() {
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["managed"])
            .with_blocked_read("managed")
            .with_refresh_drain_barrier();
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
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
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_orphaned_resources(["orphan"])
            .with_blocked_read("orphan")
            .with_refresh_drain_barrier();
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
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
        let _env_guard = crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK
            .lock()
            .await;
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["z_blocked", "a_completed"])
            .with_blocked_delete("z_blocked");
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
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
        let fixture = DestroyCancellationFixture::new()
            .with_existing_resources(["ready"])
            .with_delete_result_barrier();
        let shutdown = fixture.cancel_token();

        let destroy = run_destroy(
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

    /// A mock provider whose `read()` returns a sequence of results.
    struct SequenceProvider {
        /// Each call to `read()` pops the next result from this list.
        /// When exhausted, returns `State::not_found`.
        call_count: AtomicUsize,
        responses: Vec<ProviderResult<State>>,
    }

    impl SequenceProvider {
        fn new(responses: Vec<ProviderResult<State>>) -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                responses,
            }
        }
    }

    impl Provider for SequenceProvider {
        fn name(&self) -> &str {
            "sequence-mock"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
            _request: carina_core::provider::ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let id = id.clone();
            Box::pin(async move {
                if idx < self.responses.len() {
                    // Recreate the result since ProviderResult is not Clone
                    match &self.responses[idx] {
                        Ok(state) => Ok(state.clone()),
                        Err(e) => Err(ProviderError::api_error(e.message().to_string())),
                    }
                } else {
                    Ok(State::not_found(id))
                }
            })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            self.read(&resource.id, None, carina_core::provider::ReadRequest)
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: carina_core::provider::CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
            Box::pin(async { unreachable!() })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: carina_core::provider::UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { unreachable!() })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: carina_core::provider::DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { unreachable!() })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
        }
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
            State::existing(resource.id.clone(), HashMap::new()).with_identifier("vpc-new"),
        )]);
        let deposed_key = DeposedKey::new_unique();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.binding = Some("main".to_string());
        row.deposed.push(DeposedInstance {
            key: deposed_key.clone(),
            identifier: "vpc-old".to_string(),
            provider_instance: Some("deposed".to_string()),
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::from(["subnet".to_string()]),
        });
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let effects = build_destroy_delete_effects(
            &[&resource],
            &current_states,
            Some(&state_file),
            &HashSet::new(),
        );

        assert_eq!(effects.len(), 2);
        assert!(matches!(
            &effects[0],
            Effect::Delete {
                identifier,
                generation: EffectGeneration::Current,
                ..
            } if identifier == "vpc-new"
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
                && identifier == "vpc-old"
                && key == &deposed_key
                && dependencies == &HashSet::from(["subnet".to_string()])
        ));
    }

    #[test]
    fn build_destroy_delete_effects_skips_deposed_generations_for_protected_rows() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let resource =
            Resource::with_provider("awscc", "s3.Bucket", "state", None).with_binding("state");
        let mut row = ResourceState::new("s3.Bucket", "state", "awscc");
        row.binding = Some("state".to_string());
        row.deposed.push(DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: "old-state-bucket".to_string(),
            provider_instance: None,
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        });
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");
        let protected = HashSet::from([state_row_key_from_id(&resource.id)]);

        let effects =
            build_destroy_delete_effects(&[], &HashMap::new(), Some(&state_file), &protected);

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
        row.deposed.push(DeposedInstance {
            key: deposed_key.clone(),
            identifier: "vpc-old".to_string(),
            provider_instance: None,
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        });
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let effects =
            build_destroy_delete_effects(&[], &HashMap::new(), Some(&state_file), &HashSet::new());

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::Delete {
                identifier,
                generation: EffectGeneration::Deposed(key),
                ..
            } if identifier == "vpc-old" && key == &deposed_key
        ));
    }

    #[test]
    fn deposed_only_destroy_work_prevents_all_protected_short_circuit() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.deposed.push(DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: "vpc-old".to_string(),
            provider_instance: None,
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        });
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let effects =
            build_destroy_delete_effects(&[], &HashMap::new(), Some(&state_file), &HashSet::new());

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
        state
            .exports
            .insert("vpc_id".to_string(), serde_json::json!("vpc-12345"));

        let destroyed = vec![DestroyedInstance {
            id: ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None),
            generation: EffectGeneration::Current,
        }];
        apply_destroy_to_state(&mut state, &destroyed);

        assert!(state.resources().is_empty(), "resource should be removed");
        assert!(state.exports.is_empty(), "exports should be cleared");
    }

    #[test]
    fn apply_destroy_to_state_preserves_deposed_generations() {
        use carina_state::{DeposedInstance, DeposedKey, ResourceState};
        use std::collections::{BTreeSet, HashMap};

        let mut state = carina_state::StateFile::new();
        let mut resource = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-new")
            .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
        resource.deposed.push(DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: "vpc-old".to_string(),
            provider_instance: None,
            attributes: HashMap::from([(
                "cidr_block".to_string(),
                serde_json::json!("10.0.0.0/16"),
            )]),
            dependency_bindings: BTreeSet::from(["network".to_string()]),
        });
        state
            .upsert_resource(resource)
            .expect("test state setup must be valid");
        state
            .exports
            .insert("vpc_id".to_string(), serde_json::json!("vpc-new"));

        let destroyed = vec![DestroyedInstance {
            id: ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None),
            generation: EffectGeneration::Current,
        }];
        apply_destroy_to_state(&mut state, &destroyed);

        let retained = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("row with deposed generations should be retained");
        assert_eq!(retained.identifier, None);
        assert!(retained.attributes.is_empty());
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
        let mut resource = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-new")
            .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
        resource.deposed.push(DeposedInstance {
            key: removed_key.clone(),
            identifier: "vpc-old".to_string(),
            provider_instance: None,
            attributes: HashMap::from([(
                "cidr_block".to_string(),
                serde_json::json!("10.0.0.0/16"),
            )]),
            dependency_bindings: BTreeSet::from(["network".to_string()]),
        });
        resource.deposed.push(DeposedInstance {
            key: retained_key.clone(),
            identifier: "vpc-older".to_string(),
            provider_instance: None,
            attributes: HashMap::from([(
                "cidr_block".to_string(),
                serde_json::json!("10.2.0.0/16"),
            )]),
            dependency_bindings: BTreeSet::from(["network".to_string()]),
        });
        state
            .upsert_resource(resource)
            .expect("test state setup must be valid");

        let destroyed = vec![DestroyedInstance {
            id: ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None),
            generation: EffectGeneration::Deposed(removed_key),
        }];
        apply_destroy_to_state(&mut state, &destroyed);

        let retained = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain after deposed-only destroy result");
        assert_eq!(retained.identifier.as_deref(), Some("vpc-new"));
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
        let mut resource = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-new")
            .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
        resource.deposed.push(DeposedInstance {
            key: deposed_key.clone(),
            identifier: "vpc-old".to_string(),
            provider_instance: None,
            attributes: HashMap::from([(
                "cidr_block".to_string(),
                serde_json::json!("10.0.0.0/16"),
            )]),
            dependency_bindings: BTreeSet::from(["network".to_string()]),
        });
        state
            .upsert_resource(resource)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
        let destroyed = vec![
            DestroyedInstance {
                id: id.clone(),
                generation: EffectGeneration::Current,
            },
            DestroyedInstance {
                id,
                generation: EffectGeneration::Deposed(deposed_key),
            },
        ];
        apply_destroy_to_state(&mut state, &destroyed);

        assert!(
            state.find_resource("awscc", "ec2.Vpc", "main").is_none(),
            "row should drop only after both current and deposed generations are removed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_deletion_abandons_when_cleanup_is_prioritized() {
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let existing_state = State::existing(id.clone(), HashMap::new());
        let provider = SequenceProvider::new(vec![Ok(existing_state)]);
        let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        let poll_interval = std::time::Duration::from_secs(30);

        let wait = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            100,
            poll_interval,
        );
        tokio::pin!(wait);

        // Poll once to enter the first cleanup-aware wait and register its
        // sleep without allowing paused time to advance.
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(wait.as_mut(), cx).is_pending(),
                "wait_for_deletion returned before its first poll was in flight"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(
            provider.call_count.load(Ordering::SeqCst),
            0,
            "the first provider read must still be waiting on poll_interval"
        );

        let cleanup_requested_at = tokio::time::Instant::now();
        trigger.prioritize_cleanup();
        let result = wait.await;
        let cleanup_elapsed = cleanup_requested_at.elapsed();

        assert_eq!(result, WaitResult::Abandoned);
        assert!(
            cleanup_elapsed < poll_interval,
            "cleanup priority waited for the full poll interval: {cleanup_elapsed:?}"
        );
        assert_eq!(
            provider.call_count.load(Ordering::SeqCst),
            0,
            "cleanup priority must abandon the in-flight sleep before provider.read"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_deletion_abandons_before_polling_when_cleanup_is_already_prioritized() {
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let provider = SequenceProvider::new(Vec::new());
        let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();
        let cleanup_loop = shutdown.cleanup_aware_loop();
        trigger.prioritize_cleanup();

        let started_at = tokio::time::Instant::now();
        let result = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            180,
            std::time::Duration::from_secs(10),
        )
        .await;

        assert_eq!(
            result,
            WaitResult::Abandoned,
            "already-active cleanup must abandon before entering the poll loop"
        );
        assert_eq!(
            started_at.elapsed(),
            std::time::Duration::ZERO,
            "an already-abandoned wait must return without advancing poll time"
        );
        assert_eq!(
            provider.call_count.load(Ordering::SeqCst),
            0,
            "an already-abandoned wait must not call provider.read"
        );
    }

    #[tokio::test]
    async fn wait_for_deletion_running_token_polls_to_completion() {
        // Resource exists on first poll, then disappears on second.
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let existing_state = State::existing(id.clone(), HashMap::new());
        let provider =
            SequenceProvider::new(vec![Ok(existing_state), Ok(State::not_found(id.clone()))]);
        let shutdown = ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();

        let result = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, WaitResult::Deleted);
        assert_eq!(provider.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn wait_for_deletion_succeeds_when_resource_disappears() {
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let provider = SequenceProvider::new(vec![Ok(State::not_found(id.clone()))]);
        let shutdown = ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();

        let result = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, WaitResult::Deleted);
    }

    #[tokio::test]
    async fn wait_for_deletion_returns_read_error_on_provider_error() {
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let provider = SequenceProvider::new(vec![Err(ProviderError::api_error("auth expired"))]);
        let shutdown = ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();

        let result = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        match result {
            WaitResult::ReadError(msg) => assert!(
                msg.contains("auth expired"),
                "Expected error message to contain 'auth expired', got: {}",
                msg
            ),
            other => panic!("Expected ReadError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn wait_for_deletion_does_not_treat_read_error_as_success() {
        // This is the core regression test for issue #843.
        // Previously, Err(_) from provider.read() was treated as successful
        // deletion, causing live infrastructure to be orphaned while the user
        // was told it was destroyed.
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let provider = SequenceProvider::new(vec![Err(ProviderError::timeout("network timeout"))]);
        let shutdown = ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();

        let result = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        // Must NOT be Deleted -- that was the old (buggy) behavior
        assert!(
            matches!(&result, WaitResult::ReadError(_)),
            "Expected ReadError, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_deletion_times_out_when_resource_keeps_existing() {
        let id = ResourceId::with_identity("s3.Bucket", "test");
        let existing_state = State::existing(id.clone(), HashMap::new());
        let provider = SequenceProvider::new(vec![
            Ok(existing_state.clone()),
            Ok(existing_state.clone()),
            Ok(existing_state),
        ]);
        let shutdown = ShutdownToken::running();
        let cleanup_loop = shutdown.cleanup_aware_loop();

        let result = wait_for_deletion(
            &cleanup_loop,
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, WaitResult::TimedOut);
    }
}
