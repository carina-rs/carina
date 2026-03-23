use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

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
use crate::commands::apply::{SPINNER_FRAMES, apply_name_overrides, format_duration};
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

    // Build reverse dependency map for wait-for-completion logic
    let dependents_map = build_dependents_map(&resources_to_destroy);

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut destroyed_ids: Vec<ResourceId> = Vec::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    // timed_out_resources: binding -> (ResourceId, identifier)
    let mut timed_out_resources: HashMap<String, (ResourceId, String)> = HashMap::new();

    let destroy_total = resources_to_destroy.len();
    let mut destroy_completed: usize = 0;
    let is_tty = std::io::stdout().is_terminal();
    let spinner_stop = Arc::new(AtomicBool::new(false));
    let mut spinner_thread: Option<std::thread::JoinHandle<()>> = None;
    let mut last_inflight_len: usize = 0;

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
        let effect = Effect::Delete {
            id: resource.id.clone(),
            identifier: identifier.clone(),
            lifecycle: resource.lifecycle.clone(),
            binding,
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

        destroy_completed += 1;

        // Check if any dependent has actually failed (non-timeout)
        if let Some(failed_dep) = find_failed_dependent(&binding, &dependents_map, &failed_bindings)
        {
            let counter = format!("{}/{}", destroy_completed, destroy_total).dimmed();
            println!(
                "  {} {} - skipped (dependent {} failed) {}",
                "⊘".yellow(),
                format_effect(&effect),
                failed_dep,
                counter
            );
            skip_count += 1;
            continue;
        }

        // Check if any dependent timed out -- wait for it to complete
        let timed_out_deps: Vec<String> = dependents_map
            .get(&binding)
            .map(|deps| {
                deps.iter()
                    .filter(|d| timed_out_resources.contains_key(d.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let mut wait_failed = false;
        for dep_binding in &timed_out_deps {
            if let Some((dep_id, dep_identifier)) = timed_out_resources.remove(dep_binding.as_str())
            {
                println!(
                    "  {} Waiting for {} to be deleted...",
                    "⏳".yellow(),
                    dep_id
                );

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
                        println!(
                            "  {} Delete {} (completed after extended wait)",
                            "✓".green(),
                            dep_id
                        );
                        destroyed_ids.push(dep_id.clone());
                        success_count += 1;
                    }
                    WaitResult::ReadError(msg) => {
                        println!("  {} Delete {}", "✗".red(), dep_id);
                        println!(
                            "      {} {}",
                            "→".red(),
                            format!("read error during wait: {}", msg).red()
                        );
                        failed_bindings.insert(dep_binding.clone());
                        failure_count += 1;
                        wait_failed = true;
                    }
                    WaitResult::TimedOut => {
                        println!("  {} Delete {}", "✗".red(), dep_id);
                        println!(
                            "      {} {}",
                            "→".red(),
                            "still exists after extended wait".red()
                        );
                        failed_bindings.insert(dep_binding.clone());
                        failure_count += 1;
                        wait_failed = true;
                    }
                }
            }
        }

        if wait_failed {
            let counter = format!("{}/{}", destroy_completed, destroy_total).dimmed();
            println!(
                "  {} {} - skipped (dependent deletion did not complete) {}",
                "⊘".yellow(),
                format_effect(&effect),
                counter
            );
            skip_count += 1;
            continue;
        }

        // Show animated spinner
        if is_tty {
            let desc = format_effect(&effect).to_string();
            last_inflight_len = 2 + SPINNER_FRAMES[0].len() + 1 + desc.len() + 3;
            spinner_stop.store(false, Ordering::Relaxed);
            let stop = spinner_stop.clone();
            spinner_thread = Some(std::thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    let frame = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
                    print!("\r  \x1b[36m{}\x1b[0m {}...", frame, desc);
                    std::io::stdout().flush().ok();
                    std::thread::sleep(Duration::from_millis(80));
                    i += 1;
                }
            }));
        }

        let started = Instant::now();
        let delete_result = provider
            .delete(&resource.id, &identifier, &resource.lifecycle)
            .await;

        // Stop spinner and clear line
        spinner_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = spinner_thread.take() {
            handle.join().ok();
        }
        if is_tty && last_inflight_len > 0 {
            print!("\r{}\r", " ".repeat(last_inflight_len));
            last_inflight_len = 0;
        }

        let counter = format!("{}/{}", destroy_completed, destroy_total).dimmed();
        match delete_result {
            Ok(()) => {
                let timing = format!("[{}]", format_duration(started.elapsed())).dimmed();
                println!(
                    "  {} {} {} {}",
                    "✓".green(),
                    format_effect(&effect),
                    timing,
                    counter
                );
                success_count += 1;
                destroyed_ids.push(resource.id.clone());
            }
            Err(e) if e.is_timeout => {
                println!(
                    "  {} {} - Operation timed out, waiting for completion...",
                    "⏳".yellow(),
                    format_effect(&effect)
                );
                timed_out_resources
                    .insert(binding.clone(), (resource.id.clone(), identifier.clone()));
            }
            Err(e) => {
                let timing = format!("[{}]", format_duration(started.elapsed())).dimmed();
                println!(
                    "  {} {} {} {}",
                    "✗".red(),
                    format_effect(&effect),
                    timing,
                    counter
                );
                println!("      {} {}", "→".red(), e.to_string().red());
                failure_count += 1;
                failed_bindings.insert(binding.clone());
            }
        }
    }

    // Handle any remaining timed-out resources that no parent waited on
    for (dep_binding, (dep_id, dep_identifier)) in &timed_out_resources {
        println!(
            "  {} Waiting for {} to be deleted...",
            "⏳".yellow(),
            dep_id
        );

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
                println!(
                    "  {} Delete {} (completed after extended wait)",
                    "✓".green(),
                    dep_id
                );
                destroyed_ids.push(dep_id.clone());
                success_count += 1;
            }
            WaitResult::ReadError(msg) => {
                println!("  {} Delete {}", "✗".red(), dep_id);
                println!(
                    "      {} {}",
                    "→".red(),
                    format!("read error during wait: {}", msg).red()
                );
                failed_bindings.insert(dep_binding.clone());
                failure_count += 1;
            }
            WaitResult::TimedOut => {
                println!("  {} Delete {}", "✗".red(), dep_id);
                println!(
                    "      {} {}",
                    "→".red(),
                    "still exists after extended wait".red()
                );
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
