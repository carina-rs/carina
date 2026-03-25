use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::{
    build_dependents_map, find_failed_dependent, get_resource_dependencies,
    sort_resources_for_destroy,
};
use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::provider::Provider;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_state::{
    BackendConfig as StateBackendConfig, LockInfo, StateBackend, create_backend,
    create_local_backend,
};

use super::validate_and_resolve;
use crate::DetailLevel;
use crate::commands::apply::{apply_name_overrides, format_duration, spinner_style};
use crate::commands::state::map_lock_error;
use crate::display::{format_destroy_plan, format_effect};
use crate::error::AppError;
use crate::wiring::{
    WiringContext, get_provider_with_ctx, reconcile_anonymous_identifiers_with_ctx,
    reconcile_prefixed_names,
};

pub async fn run_destroy(
    path: &PathBuf,
    auto_approve: bool,
    lock: bool,
    refresh: bool,
) -> Result<(), AppError> {
    let mut parsed = load_configuration(path)?.parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve(&mut parsed, base_dir, true)?;

    // Don't exit early when resources are empty -- orphaned resources in the
    // state file may still need to be destroyed.

    // Check for backend configuration - use local backend by default
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(AppError::Backend)?
    } else {
        create_local_backend()
    };

    let mut protected_bucket: Option<String> = None;

    // Get the state bucket name for protection check (S3 backend only)
    if let Some(config) = backend_config {
        protected_bucket = config.attributes.get("bucket").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });
    }

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
    )
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = backend.release_lock(li).await.map_err(AppError::Backend);

        if release_result.is_ok() && op_result.is_ok() {
            println!("  {} Lock released", "✓".green());
        }

        op_result?;
        release_result
    } else {
        op_result
    }
}

async fn run_destroy_locked(
    parsed: &mut carina_core::parser::ParsedFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    protected_bucket: Option<String>,
    lock: Option<&LockInfo>,
    refresh: bool,
) -> Result<(), AppError> {
    let ctx = WiringContext::new();

    // Read current state from backend
    let state_file = backend.read_state().await.map_err(AppError::Backend)?;

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    if let Some(sf) = state_file.as_ref() {
        reconcile_anonymous_identifiers_with_ctx(&ctx, &mut parsed.resources, sf);
    }
    apply_name_overrides(&mut parsed.resources, &state_file);

    // Collect all resources (managed + orphans) before sorting.
    // We use the unsorted list for state reads, then sort once at the end.
    let mut all_resources: Vec<Resource> = parsed.resources.clone();

    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached state (--refresh=false). Plan may not reflect actual infrastructure.".yellow()
        );
    }

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(&ctx, parsed).await;

    // Build current states -- either from provider (refresh=true) or from state file
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();

    if refresh {
        // Read states for managed resources using identifier from state
        // Skip data sources (read-only) -- they won't be destroyed
        for resource in &all_resources {
            if resource.read_only {
                continue;
            }
            let identifier = state_file
                .as_ref()
                .and_then(|sf| sf.get_identifier_for_resource(resource));
            let state = provider
                .read(&resource.id, identifier.as_deref())
                .await
                .map_err(AppError::Provider)?;
            current_states.insert(resource.id.clone(), state);
        }

        // Include orphaned resources (in state but not in .crn).
        // Refresh each orphan via provider.read() to verify it still exists.
        if let Some(sf) = state_file.as_ref() {
            let desired_ids: HashSet<ResourceId> =
                all_resources.iter().map(|r| r.id.clone()).collect();
            for (id, state) in sf.build_orphan_states(&desired_ids) {
                let refreshed = provider
                    .read(&id, state.identifier.as_deref())
                    .await
                    .map_err(AppError::Provider)?;
                if refreshed.exists {
                    current_states.insert(id.clone(), refreshed);
                    let orphan_resource = build_orphan_resource(sf, &id);
                    all_resources.push(orphan_resource);
                }
            }
        }
    } else if let Some(sf) = state_file.as_ref() {
        // --refresh=false: build states from state file without AWS calls
        for resource in &all_resources {
            if resource.read_only {
                continue;
            }
            let state = sf.build_state_for_resource(resource);
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

    // Sort all resources (managed + orphans) for destroy ordering.
    // Uses depth-based pre-sorting to ensure stable ordering for independent
    // branches, then reverses for destroy order (dependents before dependencies).
    let destroy_order: Vec<Resource> =
        sort_resources_for_destroy(&all_resources).map_err(AppError::Config)?;

    // Collect resources that exist and will be destroyed
    // Skip the state bucket if it matches the backend bucket
    let mut protected_resources: Vec<&Resource> = Vec::new();
    let resources_to_destroy: Vec<&Resource> = destroy_order
        .iter()
        .filter(|r| {
            // Skip data sources (read-only resources) -- nothing to destroy
            if r.read_only {
                return false;
            }

            if !current_states.get(&r.id).map(|s| s.exists).unwrap_or(false) {
                return false;
            }

            // Check if this is the protected state bucket
            if let Some(backend_rt) = backend.resource_type()
                && r.id.resource_type == backend_rt
                && let Some(ref bucket_name) = protected_bucket
                && let Some(Value::String(name)) = r.attributes.get("bucket")
                && name == bucket_name
            {
                protected_resources.push(r);
                return false;
            }

            true
        })
        .collect();

    if resources_to_destroy.is_empty() && protected_resources.is_empty() {
        println!("{}", "No resources to destroy.".green());
        return Ok(());
    }

    // Build a Plan from the delete effects for tree display
    let mut destroy_plan = Plan::new();
    for resource in &resources_to_destroy {
        let identifier = current_states
            .get(&resource.id)
            .and_then(|s| s.identifier.clone())
            .unwrap_or_default();
        let binding = resource.attributes.get("_binding").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let dependencies = get_resource_dependencies(resource);
        destroy_plan.add(Effect::Delete {
            id: resource.id.clone(),
            identifier,
            lifecycle: resource.lifecycle.clone(),
            binding,
            dependencies,
        });
    }

    // Build delete attributes map from current states for display
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = resources_to_destroy
        .iter()
        .filter_map(|r| {
            current_states
                .get(&r.id)
                .map(|s| (r.id.clone(), s.attributes.clone()))
        })
        .collect();

    // Display destroy plan as a dependency tree
    print!(
        "{}",
        format_destroy_plan(&destroy_plan, DetailLevel::Full, &delete_attributes)
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

    println!();
    let total_count = resources_to_destroy.len() + protected_resources.len();
    if !protected_resources.is_empty() {
        println!(
            "Plan: {} to destroy, {} protected.",
            resources_to_destroy.len().to_string().red(),
            protected_resources.len().to_string().yellow()
        );
    } else {
        println!("Plan: {} to destroy.", total_count.to_string().red());
    }
    println!();

    if resources_to_destroy.is_empty() {
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
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

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
        multi.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    // Map from resource index to its spinner
    let mut spinners: HashMap<usize, ProgressBar> = HashMap::new();

    // Build reverse dependency map: binding -> {bindings that depend on it}.
    // For destroy ordering, a resource can only be deleted after ALL its
    // dependents (resources that reference it) have been deleted first.
    let dependents_map = build_dependents_map(&resources_to_destroy);

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut destroyed_ids: Vec<ResourceId> = Vec::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    // timed_out_resources: binding -> (ResourceId, identifier)
    let mut timed_out_resources: HashMap<String, (ResourceId, String)> = HashMap::new();

    let destroy_total = resources_to_destroy.len();
    let completed_counter = AtomicUsize::new(0);

    // Pre-compute binding and effect for each resource by index
    let resource_info: Vec<(String, String, Effect)> = resources_to_destroy
        .iter()
        .map(|resource| {
            let identifier = current_states
                .get(&resource.id)
                .and_then(|s| s.identifier.clone())
                .unwrap_or_default();
            let binding_for_effect = resource.attributes.get("_binding").and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            });
            let dependencies = get_resource_dependencies(resource);
            let effect = Effect::Delete {
                id: resource.id.clone(),
                identifier: identifier.clone(),
                lifecycle: resource.lifecycle.clone(),
                binding: binding_for_effect,
                dependencies,
            };
            let binding = resource
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));
            (binding, identifier, effect)
        })
        .collect();

    // Build binding -> index mapping for ready-queue scheduling
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    for (idx, (binding, _, _)) in resource_info.iter().enumerate() {
        binding_to_idx.insert(binding.clone(), idx);
    }

    // Build deletion dependency map: for each index, which indices must complete
    // before this resource can be deleted. A resource's deletion prerequisites
    // are its dependents (resources that reference it).
    let mut deletion_deps: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (idx, (binding, _, _)) in resource_info.iter().enumerate() {
        let mut deps = HashSet::new();
        if let Some(dependents) = dependents_map.get(binding) {
            for dependent_binding in dependents {
                if let Some(&dep_idx) = binding_to_idx.get(dependent_binding) {
                    deps.insert(dep_idx);
                }
            }
        }
        deletion_deps.insert(idx, deps);
    }

    // Track completed and dispatched indices
    let mut completed_indices: HashSet<usize> = HashSet::new();
    let mut dispatched: HashSet<usize> = HashSet::new();
    // Track which effects have had a waiting indicator shown
    let mut waiting_spinners: HashMap<usize, ProgressBar> = HashMap::new();
    let all_indices: Vec<usize> = (0..resources_to_destroy.len()).collect();

    // Track retry counts for dependency-violation retries
    let max_retries: usize = 3;
    let mut retry_counts: HashMap<usize, usize> = HashMap::new();
    // Indices waiting for at least one other effect to complete before retrying.
    // They are moved back to the ready pool when `in_flight.next()` returns.
    let mut retry_pending: HashSet<usize> = HashSet::new();

    let mut in_flight = FuturesUnordered::new();

    loop {
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
        newly_ready.sort();

        // Show waiting indicators for effects with unmet dependencies
        for &idx in &all_indices {
            if dispatched.contains(&idx)
                || retry_pending.contains(&idx)
                || newly_ready.contains(&idx)
            {
                continue;
            }
            let deps = &deletion_deps[&idx];
            let pending: Vec<String> = deps
                .iter()
                .filter(|d| !completed_indices.contains(d))
                .map(|d| resource_info[*d].0.clone())
                .collect();
            if !pending.is_empty() {
                let (_, _, effect) = &resource_info[idx];
                let dep_list = pending.join(", ");
                let msg = format!(
                    "{} {} {}",
                    "⏳",
                    format_effect(effect),
                    format!("[waiting for: {}]", dep_list).dimmed()
                );
                if let Some(pb) = waiting_spinners.get(&idx) {
                    pb.set_message(msg);
                } else {
                    let pb = multi.add(ProgressBar::new_spinner());
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.set_message(msg);
                    waiting_spinners.insert(idx, pb);
                }
            }
        }

        // Process newly ready resources
        for idx in newly_ready {
            dispatched.insert(idx);

            // Transition waiting indicator to spinner if it exists
            if let Some(pb) = waiting_spinners.remove(&idx) {
                let (_, _, effect) = &resource_info[idx];
                pb.set_style(spinner_style());
                pb.set_message(format_effect(effect).to_string());
                pb.enable_steady_tick(Duration::from_millis(80));
                spinners.insert(idx, pb);
            }

            let (binding, identifier, effect) = &resource_info[idx];
            let resource = resources_to_destroy[idx];

            // Check if any dependent has actually failed (non-timeout)
            if let Some(failed_dep) =
                find_failed_dependent(binding, &dependents_map, &failed_bindings)
            {
                let c = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
                let counter = format!("{}/{}", c, destroy_total).dimmed();
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
                    multi.println(format!("  {}", msg)).ok();
                }
                skip_count += 1;
                failed_bindings.insert(binding.clone());
                completed_indices.insert(idx);
                continue;
            }

            // Check if any dependent timed out -- wait for it to complete
            let timed_out_deps: Vec<String> = dependents_map
                .get(binding)
                .map(|deps| {
                    deps.iter()
                        .filter(|d| timed_out_resources.contains_key(d.as_str()))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            let mut wait_failed = false;
            for dep_binding in &timed_out_deps {
                if let Some((dep_id, dep_identifier)) =
                    timed_out_resources.remove(dep_binding.as_str())
                {
                    multi
                        .println(format!(
                            "  {} Waiting for {} to be deleted...",
                            "⏳".yellow(),
                            dep_id
                        ))
                        .ok();

                    match wait_for_deletion(
                        &provider,
                        &dep_id,
                        &dep_identifier,
                        180,
                        std::time::Duration::from_secs(10),
                    )
                    .await
                    {
                        WaitResult::Deleted => {
                            multi
                                .println(format!(
                                    "  {} Delete {} (completed after extended wait)",
                                    "✓".green(),
                                    dep_id
                                ))
                                .ok();
                            destroyed_ids.push(dep_id.clone());
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
                            failed_bindings.insert(dep_binding.clone());
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
                            failed_bindings.insert(dep_binding.clone());
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
                    multi.println(format!("  {}", msg)).ok();
                }
                skip_count += 1;
                failed_bindings.insert(binding.clone());
                completed_indices.insert(idx);
                continue;
            }

            // Create spinner for this in-flight deletion (reuse if already transitioned from waiting)
            spinners.entry(idx).or_insert_with(|| {
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style());
                pb.set_message(format_effect(effect).to_string());
                pb.enable_steady_tick(Duration::from_millis(80));
                pb
            });

            // Spawn the deletion as a concurrent future
            let resource_id = resource.id.clone();
            let identifier = identifier.clone();
            let lifecycle = resource.lifecycle.clone();
            let binding = binding.clone();

            let provider_ref = &provider;
            in_flight.push(async move {
                let started = Instant::now();
                let delete_result = provider_ref
                    .delete(&resource_id, &identifier, &lifecycle)
                    .await;
                (
                    idx,
                    binding,
                    resource_id,
                    identifier,
                    started,
                    delete_result,
                )
            });
        }

        // If nothing is in flight, we're done (or stuck)
        if in_flight.is_empty() {
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
                    let (_, _, effect) = &resource_info[idx];
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
                        multi.println(format!("  {}", msg)).ok();
                    }
                    let binding = &resource_info[idx].0;
                    failed_bindings.insert(binding.clone());
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

        // Wait for the next deletion to complete
        let (finished_idx, binding, resource_id, identifier, started, delete_result) =
            in_flight.next().await.unwrap();
        completed_indices.insert(finished_idx);

        // An effect completed — release all retry-pending indices so they
        // become eligible in the next iteration's ready-check.
        retry_pending.clear();

        let c = completed_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let counter = format!("{}/{}", c, destroy_total).dimmed();
        let effect = &resource_info[finished_idx].2;

        // Helper to finish the spinner for the completed effect
        let finish_spinner =
            |spinners: &mut HashMap<usize, ProgressBar>, idx: usize, msg: String| {
                if let Some(pb) = spinners.remove(&idx) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg);
                } else {
                    multi.println(format!("  {}", msg)).ok();
                }
            };

        match delete_result {
            Ok(()) => {
                let timing = format!("[{}]", format_duration(started.elapsed())).dimmed();
                let msg = format!(
                    "{} {} {} {}",
                    "✓".green(),
                    format_effect(effect),
                    timing,
                    counter
                );
                finish_spinner(&mut spinners, finished_idx, msg);
                success_count += 1;
                destroyed_ids.push(resource_id);
            }
            Err(e) if e.is_timeout => {
                let msg = format!(
                    "{} {} - Operation timed out, waiting for completion...",
                    "⏳".yellow(),
                    format_effect(effect)
                );
                finish_spinner(&mut spinners, finished_idx, msg);
                timed_out_resources.insert(binding.clone(), (resource_id, identifier));
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
                    // Put back as pending for retry, but gate it: the index
                    // stays in `retry_pending` until at least one other
                    // in-flight effect completes, preventing immediate
                    // re-dispatch that would fail again instantly.
                    *retry_counts.entry(finished_idx).or_insert(0) += 1;
                    completed_indices.remove(&finished_idx);
                    dispatched.remove(&finished_idx);
                    retry_pending.insert(finished_idx);
                    // Undo the counter increment since this isn't truly completed
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
                    let timing = format!("[{}]", format_duration(started.elapsed())).dimmed();
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
                    failed_bindings.insert(binding.clone());
                }
            }
        }
    }

    // Handle any remaining timed-out resources that no parent waited on
    for (dep_binding, (dep_id, dep_identifier)) in &timed_out_resources {
        multi
            .println(format!(
                "  {} Waiting for {} to be deleted...",
                "⏳".yellow(),
                dep_id
            ))
            .ok();

        match wait_for_deletion(
            &provider,
            dep_id,
            dep_identifier,
            180,
            std::time::Duration::from_secs(10),
        )
        .await
        {
            WaitResult::Deleted => {
                multi
                    .println(format!(
                        "  {} Delete {} (completed after extended wait)",
                        "✓".green(),
                        dep_id
                    ))
                    .ok();
                destroyed_ids.push(dep_id.clone());
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
                failed_bindings.insert(dep_binding.clone());
                failure_count += 1;
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
                failed_bindings.insert(dep_binding.clone());
                failure_count += 1;
            }
        }
    }

    // Save state
    println!();
    println!("{}", "Saving state...".cyan());

    // Get or create state file
    let mut state = state_file.unwrap_or_default();

    // Remove destroyed resources from state
    for id in &destroyed_ids {
        state.remove_resource(&id.provider, &id.resource_type, &id.name);
    }

    // Save state (with or without lock validation)
    if let Some(lock) = lock {
        crate::commands::apply::save_state_locked(backend, lock, &mut state).await?;
    } else {
        crate::commands::apply::save_state_unlocked(backend, &mut state).await?;
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

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

/// Build a minimal `Resource` for an orphaned resource from the state file.
///
/// This creates a Resource with attributes reconstructed from state data,
/// including `_binding` and `_dependency_bindings` so that dependency ordering
/// and tree display work correctly.
fn build_orphan_resource(sf: &carina_state::StateFile, id: &ResourceId) -> Resource {
    let rs = sf
        .find_resource(&id.provider, &id.resource_type, &id.name)
        .expect("orphan must exist in state file");
    let mut attributes: HashMap<String, Value> = rs
        .attributes
        .iter()
        .filter_map(|(k, v)| carina_core::value::json_to_dsl_value(v).map(|val| (k.clone(), val)))
        .collect();
    if let Some(ref binding) = rs.binding {
        attributes.insert("_binding".to_string(), Value::String(binding.clone()));
    }
    if !rs.dependency_bindings.is_empty() {
        attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(
                rs.dependency_bindings
                    .iter()
                    .map(|b| Value::String(b.clone()))
                    .collect(),
            ),
        );
    }
    Resource {
        id: id.clone(),
        attributes,
        read_only: false,
        lifecycle: rs.lifecycle.clone(),
        prefixes: rs.prefixes.clone(),
    }
}

/// Check if a delete error is retryable due to implicit dependency ordering.
///
/// Some AWS errors indicate that a resource cannot be deleted yet because
/// another resource still depends on it, even though there is no explicit
/// ResourceRef dependency. These errors are retryable: once the blocker is
/// deleted, the retry will succeed.
fn is_retryable_delete_error(e: &carina_core::provider::ProviderError) -> bool {
    if e.is_timeout {
        return false;
    }
    let msg = e.to_string();
    let retryable_patterns = [
        "DependencyViolation",
        "has dependent object",
        "has a dependent object",
        "resource has dependencies",
        "mapped public address",
        "Failed to detach",
    ];
    retryable_patterns.iter().any(|p| msg.contains(p))
}

/// Result of waiting for a resource deletion to complete.
#[derive(Debug, PartialEq)]
enum WaitResult {
    /// Resource confirmed deleted (`state.exists == false`).
    Deleted,
    /// A `provider.read()` call returned an error.
    ReadError(String),
    /// The resource still existed after all retry attempts.
    TimedOut,
}

/// Poll `provider.read()` in a loop until the resource disappears or an error /
/// timeout occurs.
///
/// * `max_attempts` – how many times to poll (each preceded by `poll_interval`).
/// * `poll_interval` – sleep duration between polls.
async fn wait_for_deletion(
    provider: &dyn Provider,
    id: &ResourceId,
    identifier: &str,
    max_attempts: usize,
    poll_interval: std::time::Duration,
) -> WaitResult {
    for _ in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;
        match provider.read(id, Some(identifier)).await {
            Ok(state) if !state.exists => return WaitResult::Deleted,
            Ok(_) => {
                // Still exists, keep waiting
            }
            Err(e) => return WaitResult::ReadError(e.to_string()),
        }
    }
    WaitResult::TimedOut
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::provider::{BoxFuture, ProviderError, ProviderResult};
    use carina_core::resource::LifecycleConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        fn name(&self) -> &'static str {
            "sequence-mock"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let id = id.clone();
            Box::pin(async move {
                if idx < self.responses.len() {
                    // Recreate the result since ProviderResult is not Clone
                    match &self.responses[idx] {
                        Ok(state) => Ok(state.clone()),
                        Err(e) => Err(ProviderError::new(e.message.clone())),
                    }
                } else {
                    Ok(State::not_found(id))
                }
            })
        }

        fn create(&self, _resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { unreachable!() })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _from: &State,
            _to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { unreachable!() })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { unreachable!() })
        }
    }

    #[test]
    fn is_retryable_detects_dependency_violation() {
        let err = ProviderError::new(
            "DependencyViolation: Network vpc-xxx has some mapped public address(es)",
        );
        assert!(is_retryable_delete_error(&err));
    }

    #[test]
    fn is_retryable_detects_has_dependent_object() {
        let err = ProviderError::new("resource has a dependent object");
        assert!(is_retryable_delete_error(&err));
    }

    #[test]
    fn is_retryable_returns_false_for_generic_error() {
        let err = ProviderError::new("AccessDenied: not authorized");
        assert!(!is_retryable_delete_error(&err));
    }

    #[test]
    fn is_retryable_returns_false_for_timeout() {
        let err = ProviderError::new("DependencyViolation: something").timeout();
        assert!(!is_retryable_delete_error(&err));
    }

    #[tokio::test]
    async fn wait_for_deletion_succeeds_when_resource_disappears() {
        let id = ResourceId::new("s3.bucket", "test");
        let provider = SequenceProvider::new(vec![Ok(State::not_found(id.clone()))]);

        let result = wait_for_deletion(
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
        let id = ResourceId::new("s3.bucket", "test");
        let provider = SequenceProvider::new(vec![Err(ProviderError::new("auth expired"))]);

        let result = wait_for_deletion(
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
        let id = ResourceId::new("s3.bucket", "test");
        let provider = SequenceProvider::new(vec![Err(ProviderError::new("network timeout"))]);

        let result = wait_for_deletion(
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        // Must NOT be Deleted -- that was the old (buggy) behavior
        assert_ne!(result, WaitResult::Deleted);
    }

    #[tokio::test]
    async fn wait_for_deletion_times_out_when_resource_keeps_existing() {
        let id = ResourceId::new("s3.bucket", "test");
        let existing_state = State::existing(id.clone(), HashMap::new());
        let provider = SequenceProvider::new(vec![
            Ok(existing_state.clone()),
            Ok(existing_state.clone()),
            Ok(existing_state),
        ]);

        let result = wait_for_deletion(
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, WaitResult::TimedOut);
    }

    #[tokio::test]
    async fn wait_for_deletion_succeeds_after_transient_exists() {
        // Resource exists on first poll, then disappears on second.
        let id = ResourceId::new("s3.bucket", "test");
        let existing_state = State::existing(id.clone(), HashMap::new());
        let provider =
            SequenceProvider::new(vec![Ok(existing_state), Ok(State::not_found(id.clone()))]);

        let result = wait_for_deletion(
            &provider,
            &id,
            "some-identifier",
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, WaitResult::Deleted);
    }
}
