use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer};
use carina_core::resource::{LifecycleConfig, ResourceId, State, Value};
use carina_core::value::{format_value, json_to_dsl_value};
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, LockInfo, ResourceState, StateBackend,
    create_backend, create_local_backend,
};

use super::validate_and_resolve;
use crate::commands::apply::apply_name_overrides;
use crate::error::AppError;
use crate::wiring::{
    get_provider, provider_factories, reconcile_anonymous_identifiers, reconcile_prefixed_names,
};

/// Convert a lock acquisition error into an `AppError`.
///
/// For `Locked` errors, includes a hint about `force-unlock`.
/// All other backend errors are passed through as `AppError::Backend`.
pub fn map_lock_error(e: BackendError) -> AppError {
    match e {
        BackendError::Locked {
            who,
            lock_id,
            operation,
        } => AppError::Config(format!(
            "State is locked by {} (lock ID: {}, operation: {})\n\
             If you believe this is stale, run: carina force-unlock {}",
            who, lock_id, operation, lock_id
        )),
        other => AppError::Backend(other),
    }
}

#[derive(clap::Subcommand)]
pub enum StateCommands {
    /// Delete state bucket (requires --force flag)
    BucketDelete {
        /// Name of the bucket to delete
        bucket_name: String,

        /// Force deletion without confirmation
        #[arg(long)]
        force: bool,

        /// Path to .crn file or directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Refresh state from cloud providers without planning or applying
    Refresh {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// Run state subcommands
pub async fn run_state_command(command: StateCommands) -> Result<(), AppError> {
    match command {
        StateCommands::BucketDelete {
            bucket_name,
            force,
            path,
        } => run_state_bucket_delete(&bucket_name, force, &path).await,
        StateCommands::Refresh { path } => run_state_refresh(&path).await,
    }
}

/// Run force-unlock command
pub async fn run_force_unlock(lock_id: &str, path: &PathBuf) -> Result<(), AppError> {
    let parsed = load_configuration(path)?.parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found. force-unlock requires a backend.")?;

    let state_config = StateBackendConfig::from(backend_config);
    let backend = create_backend(&state_config)
        .await
        .map_err(AppError::Backend)?;

    println!("{}", "Force unlocking state...".yellow().bold());
    println!("Lock ID: {}", lock_id);

    match backend.force_unlock(lock_id).await {
        Ok(()) => {
            println!("{}", "State has been successfully unlocked.".green().bold());
            Ok(())
        }
        Err(BackendError::LockNotFound(_)) => Err(AppError::Config(format!(
            "Lock with ID '{}' not found.",
            lock_id
        ))),
        Err(BackendError::LockMismatch { expected, actual }) => Err(AppError::Config(format!(
            "Lock ID mismatch. Expected '{}', found '{}'.",
            expected, actual
        ))),
        Err(e) => Err(AppError::Backend(e)),
    }
}

/// Run state bucket delete command
async fn run_state_bucket_delete(
    bucket_name: &str,
    force: bool,
    path: &PathBuf,
) -> Result<(), AppError> {
    let parsed = load_configuration(path)?.parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found.")?;

    // Verify the bucket name matches the backend configuration
    let config_bucket = backend_config
        .attributes
        .get("bucket")
        .and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        })
        .ok_or("Backend configuration missing 'bucket' attribute")?;

    if config_bucket != bucket_name {
        return Err(AppError::Config(format!(
            "Bucket name '{}' does not match backend configuration bucket '{}'.",
            bucket_name, config_bucket
        )));
    }

    println!(
        "{}",
        "WARNING: This will delete the state bucket and all state history."
            .red()
            .bold()
    );
    println!("Bucket: {}", bucket_name.yellow());

    if !force {
        println!();
        println!("{}", "Type the bucket name to confirm deletion:".yellow());
        print!("  Enter bucket name: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != bucket_name {
            println!();
            println!("{}", "Deletion cancelled.".yellow());
            return Ok(());
        }
    }

    // Create backend to get provider metadata
    let state_config = StateBackendConfig::from(backend_config);
    let backend = create_backend(&state_config)
        .await
        .map_err(AppError::Backend)?;

    // Get provider metadata from backend
    let backend_provider_name = backend
        .provider_name()
        .ok_or("Backend does not specify a provider name")?;
    let backend_resource_type = backend
        .resource_type()
        .ok_or("Backend does not specify a resource type")?;
    let factories = provider_factories();
    let factory = provider_mod::find_factory(&factories, backend_provider_name)
        .ok_or_else(|| format!("No provider factory found for '{}'", backend_provider_name))?;

    // Create provider to delete the bucket
    let provider_config_attrs = parsed
        .providers
        .iter()
        .find(|p| p.name == backend_provider_name)
        .map(|p| p.attributes.clone())
        .unwrap_or_default();
    let bucket_provider = factory.create_provider(&provider_config_attrs).await;

    // First, try to empty the bucket (delete all objects and versions)
    println!();
    println!("{}", "Emptying bucket...".cyan());

    // Delete the bucket resource (identifier is the bucket name)
    let bucket_id =
        ResourceId::with_provider(backend_provider_name, backend_resource_type, bucket_name);
    match bucket_provider
        .delete(&bucket_id, bucket_name, &LifecycleConfig::default())
        .await
    {
        Ok(()) => {
            println!(
                "{}",
                format!("Deleted state bucket: {}", bucket_name)
                    .green()
                    .bold()
            );
            Ok(())
        }
        Err(e) => Err(AppError::Provider(e)),
    }
}

/// Run state refresh command
pub async fn run_state_refresh(path: &PathBuf) -> Result<(), AppError> {
    let loaded = load_configuration(path)?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve(&mut parsed, base_dir, true)?;

    // Create backend
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(AppError::Backend)?
    } else {
        create_local_backend()
    };

    // Acquire lock
    println!("{}", "Acquiring state lock...".cyan());
    let lock = backend
        .acquire_lock("refresh")
        .await
        .map_err(map_lock_error)?;
    println!("  {} Lock acquired", "✓".green());

    let op_result = run_state_refresh_locked(&mut parsed, backend.as_ref(), &lock).await;

    // Always release lock, regardless of whether the operation succeeded
    let release_result = backend.release_lock(&lock).await.map_err(AppError::Backend);

    op_result?;
    release_result
}

async fn run_state_refresh_locked(
    parsed: &mut carina_core::parser::ParsedFile,
    backend: &dyn StateBackend,
    lock: &LockInfo,
) -> Result<(), AppError> {
    // Read current state from backend
    let mut state_file = backend.read_state().await.map_err(AppError::Backend)?;

    if state_file.as_ref().is_none_or(|s| s.resources.is_empty()) {
        let msg = if state_file.is_none() {
            "No state file found. Nothing to refresh."
        } else {
            "No resources in state. Nothing to refresh."
        };
        println!("{}", msg.yellow());
        return Ok(());
    }

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    reconcile_anonymous_identifiers(&mut parsed.resources, &state_file);
    apply_name_overrides(&mut parsed.resources, &state_file);

    let sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;

    // Select provider
    let provider = get_provider(parsed).await;

    println!();
    println!("{}", "Refreshing state...".cyan().bold());

    // Read states for all resources using identifier from state
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        let identifier = state_file
            .as_ref()
            .and_then(|sf| sf.get_identifier_for_resource(resource));

        // Skip resources not in state (no identifier means not managed)
        if identifier.is_none() {
            continue;
        }

        let fresh_state = provider
            .read(&resource.id, identifier.as_deref())
            .await
            .map_err(AppError::Provider)?;
        current_states.insert(resource.id.clone(), fresh_state);
    }

    // Restore unreturned attributes from state file (CloudControl doesn't always return them)
    let saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    provider.hydrate_read_state(&mut current_states, &saved_attrs);

    let mut state = state_file.take().unwrap();

    println!();

    let mut updated_count = 0u32;
    let mut unchanged_count = 0u32;

    for resource in &sorted_resources {
        let fresh_state = match current_states.get(&resource.id) {
            Some(s) => s,
            None => continue, // Not in state, skip
        };

        // Compare old state attributes with new
        let existing = state.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            &resource.id.name,
        );

        let mut has_changes = false;
        let mut changes: Vec<String> = Vec::new();

        if let Some(existing_rs) = existing {
            // Build old attributes as DSL values for comparison
            let old_attrs: HashMap<String, Value> = existing_rs
                .attributes
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect();

            if !fresh_state.exists {
                // Resource was deleted externally
                has_changes = true;
                changes.push(format!("    {} resource no longer exists", "-".red()));
            } else {
                // Check for modified and removed attributes
                let mut all_keys: HashSet<&String> = old_attrs.keys().collect();
                all_keys.extend(fresh_state.attributes.keys());

                let mut sorted_keys: Vec<&&String> = all_keys.iter().collect();
                sorted_keys.sort();

                for key in sorted_keys {
                    let old_val = old_attrs.get(*key);
                    let new_val = fresh_state.attributes.get(*key);

                    match (old_val, new_val) {
                        (Some(old), Some(new)) if old != new => {
                            has_changes = true;
                            changes.push(format!(
                                "    {} {}: {} {} {}",
                                "~".yellow(),
                                key,
                                format_value(old).red(),
                                "→".dimmed(),
                                format_value(new).green(),
                            ));
                        }
                        (Some(old), None) => {
                            has_changes = true;
                            changes.push(format!(
                                "    {} {}: {}",
                                "-".red(),
                                key,
                                format_value(old).red(),
                            ));
                        }
                        (None, Some(new)) => {
                            has_changes = true;
                            changes.push(format!(
                                "    {} {}: {}",
                                "+".green(),
                                key,
                                format_value(new).green(),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        } else {
            // Resource in config but not in state -- shouldn't happen during refresh
            continue;
        }

        if has_changes {
            updated_count += 1;
            println!(
                "  {} \"{}\":",
                resource.id.display_type().cyan(),
                resource.id.name
            );
            for change in &changes {
                println!("{}", change);
            }
            println!();
        } else {
            unchanged_count += 1;
        }

        // Update state with refreshed data
        if fresh_state.exists {
            let existing_rs = state.find_resource(
                &resource.id.provider,
                &resource.id.resource_type,
                &resource.id.name,
            );
            let resource_state =
                ResourceState::from_provider_state(resource, fresh_state, existing_rs)?;
            state.upsert_resource(resource_state);
        } else {
            state.remove_resource(
                &resource.id.provider,
                &resource.id.resource_type,
                &resource.id.name,
            );
        }
    }

    // Renew lock and save with lock validation
    crate::commands::apply::save_state_locked(backend, lock, &mut state).await?;

    // Summary
    println!(
        "State refreshed: {} resource{} updated, {} resource{} unchanged.",
        updated_count,
        if updated_count == 1 { "" } else { "s" },
        unchanged_count,
        if unchanged_count == 1 { "" } else { "s" },
    );
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    Ok(())
}
