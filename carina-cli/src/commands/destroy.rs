use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::{
    build_dependents_map, find_failed_dependent, sort_resources_by_dependencies,
};
use carina_core::effect::Effect;
use carina_core::provider::Provider;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_state::{
    BackendConfig as StateBackendConfig, StateBackend, create_backend, create_local_backend,
};

use super::validate_and_resolve;
use crate::commands::apply::apply_name_overrides;
use crate::commands::state::map_lock_error;
use crate::display::format_effect;
use crate::error::AppError;
use crate::wiring::{get_provider, reconcile_anonymous_identifiers, reconcile_prefixed_names};

pub async fn run_destroy(path: &PathBuf, auto_approve: bool) -> Result<(), AppError> {
    let mut parsed = load_configuration(path)?.parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve(&mut parsed, base_dir, true)?;

    if parsed.resources.is_empty() {
        println!("{}", "No resources defined in configuration.".yellow());
        return Ok(());
    }

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

    // Acquire lock
    println!("{}", "Acquiring state lock...".cyan());
    let lock = backend
        .acquire_lock("destroy")
        .await
        .map_err(map_lock_error)?;
    println!("  {} Lock acquired", "✓".green());

    let op_result = run_destroy_locked(
        &mut parsed,
        auto_approve,
        backend.as_ref(),
        protected_bucket,
    )
    .await;

    // Always release lock, regardless of whether the operation succeeded
    let release_result = backend.release_lock(&lock).await.map_err(AppError::Backend);

    if release_result.is_ok() && op_result.is_ok() {
        println!("  {} Lock released", "✓".green());
    }

    op_result?;
    release_result
}

async fn run_destroy_locked(
    parsed: &mut carina_core::parser::ParsedFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    protected_bucket: Option<String>,
) -> Result<(), AppError> {
    // Read current state from backend
    let state_file = backend.read_state().await.map_err(AppError::Backend)?;

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    reconcile_anonymous_identifiers(&mut parsed.resources, &state_file);
    apply_name_overrides(&mut parsed.resources, &state_file);

    // Sort resources by dependencies (for creation order)
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;

    // Reverse the order for destruction (dependents first, then dependencies)
    let destroy_order: Vec<Resource> = sorted_resources.into_iter().rev().collect();

    // Select appropriate Provider based on configuration
    let provider = get_provider(parsed).await;

    // Read states for managed resources using identifier from state
    // Skip data sources (read-only) -- they won't be destroyed
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &destroy_order {
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

    // Display destroy plan
    println!("{}", "Destroy Plan:".red().bold());
    println!();

    for resource in &resources_to_destroy {
        println!("  {} {}", "-".red().bold(), resource.id);
    }

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

    for resource in &resources_to_destroy {
        let identifier = current_states
            .get(&resource.id)
            .and_then(|s| s.identifier.clone())
            .unwrap_or_default();
        let effect = Effect::Delete {
            id: resource.id.clone(),
            identifier: identifier.clone(),
            lifecycle: resource.lifecycle.clone(),
        };

        let binding = resource
            .attributes
            .get("_binding")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));

        // Check if any dependent has actually failed (non-timeout)
        if let Some(failed_dep) = find_failed_dependent(&binding, &dependents_map, &failed_bindings)
        {
            println!(
                "  {} {} - skipped (dependent {} failed)",
                "⊘".yellow(),
                format_effect(&effect),
                failed_dep
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
                        println!(
                            "  {} {} - read error during wait: {}",
                            "✗".red(),
                            dep_id,
                            msg
                        );
                        failed_bindings.insert(dep_binding.clone());
                        failure_count += 1;
                        wait_failed = true;
                    }
                    WaitResult::TimedOut => {
                        println!(
                            "  {} {} - still exists after extended wait",
                            "✗".red(),
                            dep_id
                        );
                        failed_bindings.insert(dep_binding.clone());
                        failure_count += 1;
                        wait_failed = true;
                    }
                }
            }
        }

        if wait_failed {
            println!(
                "  {} {} - skipped (dependent deletion did not complete)",
                "⊘".yellow(),
                format_effect(&effect)
            );
            skip_count += 1;
            continue;
        }

        let delete_result = provider
            .delete(&resource.id, &identifier, &resource.lifecycle)
            .await;

        match delete_result {
            Ok(()) => {
                println!("  {} {}", "✓".green(), format_effect(&effect));
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
                println!("  {} {} - {}", "✗".red(), format_effect(&effect), e);
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
                println!(
                    "  {} {} - read error during wait: {}",
                    "✗".red(),
                    dep_id,
                    msg
                );
                failed_bindings.insert(dep_binding.clone());
                failure_count += 1;
            }
            WaitResult::TimedOut => {
                println!(
                    "  {} {} - still exists after extended wait",
                    "✗".red(),
                    dep_id
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

    // Increment serial and save
    state.increment_serial();
    backend
        .write_state(&state)
        .await
        .map_err(AppError::Backend)?;
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
