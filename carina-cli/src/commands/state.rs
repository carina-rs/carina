use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::PathBuf;

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::effect::Effect;
use carina_core::parser::ProviderContext;
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::value::{format_value, json_to_dsl_value};
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, LockInfo, ResourceState, StateBackend,
    StateFile, create_backend, create_local_backend,
};

use super::validate_and_resolve_with_config;
use crate::commands::apply::apply_name_overrides;
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, get_provider_with_ctx,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
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

/// Read local state file for shell completion.
///
/// Tries `carina.state.json` in the current directory. Returns `None` if the
/// file does not exist or cannot be parsed (completion simply produces no
/// candidates in that case).
fn read_local_state_for_completion() -> Option<StateFile> {
    let path = std::path::Path::new("carina.state.json");
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Shell completion function for `state lookup` queries.
///
/// Before the first `.`: completes binding names / resource names.
/// After the `.`: completes attribute names for the matched resource.
fn complete_state_lookup(current: &OsStr) -> Vec<CompletionCandidate> {
    let current = match current.to_str() {
        Some(s) => s,
        None => return vec![],
    };

    let state = match read_local_state_for_completion() {
        Some(s) => s,
        None => return vec![],
    };

    complete_state_lookup_from(&state, current)
}

/// Compute completion candidates from a state file and a partial query string.
fn complete_state_lookup_from(state: &StateFile, current: &str) -> Vec<CompletionCandidate> {
    if let Some((resource_name, _attr_prefix)) = current.split_once('.') {
        // Complete attribute names for the matched resource
        let rs = match find_resource_by_query(state, resource_name) {
            Some(rs) => rs,
            None => return vec![],
        };
        rs.attributes
            .keys()
            .filter(|key| {
                let full = format!("{}.{}", resource_name, key);
                full.starts_with(current)
            })
            .map(|key| CompletionCandidate::new(format!("{}.{}", resource_name, key)))
            .collect()
    } else {
        // Complete resource binding names / resource names
        let mut candidates = Vec::new();
        for rs in &state.resources {
            let display_name = rs.binding.as_deref().unwrap_or(&rs.name);
            if display_name.starts_with(current) {
                candidates.push(CompletionCandidate::new(display_name));
            }
        }
        candidates
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

        /// Path to directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Refresh state from cloud providers without planning or applying
    Refresh {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Enable/disable state locking (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        lock: bool,
    },
    /// List all managed resources from the state file
    List {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Look up resource attributes from the state file
    Lookup {
        /// Query: <binding_or_name> for full resource, <binding_or_name>.<attribute> for specific attribute
        #[arg(add = ArgValueCompleter::new(complete_state_lookup))]
        query: String,

        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Always output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show all managed resources with full attributes
    Show {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Display state in interactive TUI mode
        #[arg(long)]
        tui: bool,
    },
}

/// Run state subcommands
pub async fn run_state_command(
    command: StateCommands,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    match command {
        StateCommands::BucketDelete {
            bucket_name,
            force,
            path,
        } => run_state_bucket_delete(&bucket_name, force, &path, provider_context).await,
        StateCommands::Refresh { path, lock } => {
            run_state_refresh(&path, lock, provider_context).await
        }
        StateCommands::List { path } => run_state_list(&path, provider_context).await,
        StateCommands::Lookup { query, path, json } => {
            run_state_lookup(&query, &path, json, provider_context).await
        }
        StateCommands::Show { path, tui } => run_state_show(&path, tui, provider_context).await,
    }
}

/// Run force-unlock command
pub async fn run_force_unlock(
    lock_id: &str,
    path: &PathBuf,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let parsed = load_configuration_with_config(path, provider_context)?.parsed;

    let backend: Box<dyn StateBackend> = if let Some(config) = parsed.backend.as_ref() {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(AppError::Backend)?
    } else {
        create_local_backend()
    };

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

/// Load the state file from the backend (or local file), without acquiring a lock.
async fn load_state_file(
    path: &PathBuf,
    provider_context: &ProviderContext,
) -> Result<StateFile, AppError> {
    let loaded = load_configuration_with_config(path, provider_context)?;
    let parsed = loaded.parsed;

    let backend: Box<dyn StateBackend> = if let Some(config) = parsed.backend.as_ref() {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(AppError::Backend)?
    } else {
        create_local_backend()
    };

    let state_file = backend.read_state().await.map_err(AppError::Backend)?;
    state_file.ok_or_else(|| AppError::Config("No state file found.".to_string()))
}

/// Find a resource by binding name first, then fall back to resource name.
fn find_resource_by_query<'a>(state: &'a StateFile, name: &str) -> Option<&'a ResourceState> {
    // Search by binding first
    state
        .resources
        .iter()
        .find(|r| r.binding.as_deref() == Some(name))
        .or_else(|| {
            // Fall back to name
            state.resources.iter().find(|r| r.name == name)
        })
}

/// Format state list output. Returns each line as a string.
fn format_state_list(state: &StateFile) -> Vec<String> {
    state
        .resources
        .iter()
        .map(|rs| {
            let display_name = rs.binding.as_deref().unwrap_or(&rs.name);
            format!("{}.{} {}", rs.provider, rs.resource_type, display_name)
        })
        .collect()
}

/// Run state list command
async fn run_state_list(
    path: &PathBuf,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, provider_context).await?;

    if state.resources.is_empty() {
        println!("No resources in state.");
        return Ok(());
    }

    for line in format_state_list(&state) {
        println!("{}", line);
    }

    Ok(())
}

/// Format lookup output for a query against a state file.
/// Returns the formatted output string on success, or an error.
fn format_state_lookup(
    state: &StateFile,
    query: &str,
    json_output: bool,
) -> Result<String, AppError> {
    // Parse query: "binding" or "binding.attribute"
    let (resource_name, attribute) = match query.split_once('.') {
        Some((name, attr)) => (name, Some(attr)),
        None => (query, None),
    };

    let rs = find_resource_by_query(state, resource_name).ok_or_else(|| {
        AppError::Config(format!("Resource '{}' not found in state.", resource_name))
    })?;

    match attribute {
        Some(attr) => {
            let value = rs.attributes.get(attr).ok_or_else(|| {
                AppError::Config(format!(
                    "Attribute '{}' not found on resource '{}'.",
                    attr, resource_name
                ))
            })?;
            if json_output {
                Ok(serde_json::to_string_pretty(value).unwrap())
            } else {
                Ok(format_raw_value(value))
            }
        }
        None => {
            // Full resource: output all attributes as JSON object (sorted keys for deterministic output)
            let sorted: std::collections::BTreeMap<_, _> = rs.attributes.iter().collect();
            Ok(serde_json::to_string_pretty(&sorted).unwrap())
        }
    }
}

/// Run state lookup command
async fn run_state_lookup(
    query: &str,
    path: &PathBuf,
    json_output: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, provider_context).await?;
    let output = format_state_lookup(&state, query, json_output)?;
    println!("{}", output);
    Ok(())
}

/// Build a synthetic `Plan` from a state file for TUI display.
///
/// Each resource in the state becomes a `Read` effect so the TUI can
/// render it with all attributes in the detail panel.
fn build_plan_from_state(state: &StateFile) -> Plan {
    let mut plan = Plan::new();
    for rs in &state.resources {
        let mut resource = Resource::with_provider(&rs.provider, &rs.resource_type, &rs.name);

        // Set typed metadata fields from state
        resource.binding = rs.binding.clone();
        resource.dependency_bindings = rs.dependency_bindings.clone();

        // Convert JSON attributes to DSL values
        for (key, json_val) in &rs.attributes {
            if let Some(dsl_val) = json_to_dsl_value(json_val) {
                resource.set_attr(key.clone(), dsl_val);
            }
        }

        resource.kind = carina_core::resource::ResourceKind::DataSource;
        plan.add(Effect::Read { resource });
    }
    plan
}

/// Format state show output (non-TUI mode).
///
/// Shows all resources with their type, name/binding, and full attributes.
fn format_state_show(state: &StateFile) -> String {
    let mut output = String::new();
    for (i, rs) in state.resources.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        let display_name = rs.binding.as_deref().unwrap_or(&rs.name);
        output.push_str(&format!(
            "# {}.{} ({})\n",
            rs.provider, rs.resource_type, display_name
        ));

        // Sort attributes for deterministic output
        let mut keys: Vec<&String> = rs.attributes.keys().collect();
        keys.sort();
        for key in keys {
            let value = &rs.attributes[key];
            if let Some(dsl_val) = json_to_dsl_value(value) {
                output.push_str(&format!("  {} = {}\n", key, format_value(&dsl_val)));
            }
        }
    }
    output
}

/// Run state show command
async fn run_state_show(
    path: &PathBuf,
    tui: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, provider_context).await?;

    if state.resources.is_empty() {
        println!("No resources in state.");
        return Ok(());
    }

    if tui {
        let plan = build_plan_from_state(&state);
        carina_tui::run(&plan, &HashMap::new())
            .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
    } else {
        let output = format_state_show(&state);
        print!("{}", output);
    }

    Ok(())
}

/// Format a JSON value in raw format (no quotes for strings, suitable for shell usage).
fn format_raw_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        // Arrays and objects get JSON output
        _ => serde_json::to_string_pretty(value).unwrap(),
    }
}

/// Run state bucket delete command
async fn run_state_bucket_delete(
    bucket_name: &str,
    force: bool,
    path: &PathBuf,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let parsed = load_configuration_with_config(path, provider_context)?.parsed;

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
    let base_dir = get_base_dir(path);
    let factories = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);
    let factory = provider_mod::find_factory(ctx.factories(), backend_provider_name)
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
        .delete(
            &bucket_id,
            bucket_name,
            &carina_core::resource::LifecycleConfig::default(),
        )
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
pub async fn run_state_refresh(
    path: &PathBuf,
    lock: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(path, provider_context)?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve_with_config(&mut parsed, base_dir, true, provider_context)?;

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

    // Acquire lock (unless --lock=false)
    let lock_info: Option<LockInfo> = if lock {
        println!("{}", "Acquiring state lock...".cyan());
        let li = backend
            .acquire_lock("refresh")
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

    let op_result =
        run_state_refresh_locked(&mut parsed, backend.as_ref(), lock_info.as_ref(), base_dir).await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = backend.release_lock(li).await.map_err(AppError::Backend);

        op_result?;
        release_result
    } else {
        op_result
    }
}

pub(crate) async fn run_state_refresh_locked(
    parsed: &mut carina_core::parser::ParsedFile,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    base_dir: &std::path::Path,
) -> Result<(), AppError> {
    let factories = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);

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
    if let Some(sf) = state_file.as_ref() {
        reconcile_anonymous_identifiers_with_ctx(&ctx, &mut parsed.resources, sf);
    }
    apply_name_overrides(&mut parsed.resources, &state_file);

    let sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;

    // Select provider
    let provider = get_provider_with_ctx(&ctx, parsed, base_dir).await;

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

    // Also read states for orphaned resources (in state but removed from config)
    let desired_ids: HashSet<ResourceId> = sorted_resources.iter().map(|r| r.id.clone()).collect();
    let orphan_ids: Vec<(ResourceId, String)> = state_file
        .as_ref()
        .map(|sf| {
            sf.resources
                .iter()
                .filter_map(|rs| {
                    let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
                    if desired_ids.contains(&id) {
                        return None;
                    }
                    rs.identifier.as_ref().map(|ident| (id, ident.clone()))
                })
                .collect()
        })
        .unwrap_or_default();

    for (id, identifier) in &orphan_ids {
        let fresh_state = provider
            .read(id, Some(identifier.as_str()))
            .await
            .map_err(AppError::Provider)?;
        current_states.insert(id.clone(), fresh_state);
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
        diff_display_update_resource(
            &resource.id,
            fresh_state,
            &mut state,
            Some(resource),
            "",
            &mut updated_count,
            &mut unchanged_count,
        )?;
    }

    // Process orphaned resources (in state but removed from config)
    for (orphan_id, _) in &orphan_ids {
        let fresh_state = match current_states.get(orphan_id) {
            Some(s) => s,
            None => continue,
        };
        diff_display_update_resource(
            orphan_id,
            fresh_state,
            &mut state,
            None,
            " (orphan)",
            &mut updated_count,
            &mut unchanged_count,
        )?;
    }

    // Save state (with or without lock validation)
    if let Some(lock) = lock {
        crate::commands::apply::save_state_locked(backend, lock, &mut state).await?;
    } else {
        crate::commands::apply::save_state_unlocked(backend, &mut state).await?;
    }

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

/// Compare old state with fresh provider state for a single resource,
/// display any changes, and update the state file accordingly.
///
/// When `resource` is `Some`, lifecycle config, prefixes, and desired keys
/// are preserved from it. When `None` (orphan resources), a minimal
/// `Resource` is constructed from the id.
///
/// `label_suffix` is appended to the resource header (e.g., `" (orphan)"`).
fn diff_display_update_resource(
    id: &ResourceId,
    fresh_state: &State,
    state: &mut carina_state::StateFile,
    resource: Option<&Resource>,
    label_suffix: &str,
    updated_count: &mut u32,
    unchanged_count: &mut u32,
) -> Result<(), AppError> {
    let existing = state.find_resource(&id.provider, &id.resource_type, &id.name);
    let existing_rs = match existing {
        Some(rs) => rs,
        None => return Ok(()),
    };

    // Build old attributes as DSL values for comparison
    let old_attrs: HashMap<String, Value> = existing_rs
        .attributes
        .iter()
        .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
        .collect();

    let mut has_changes = false;
    let mut changes: Vec<String> = Vec::new();

    if !fresh_state.exists {
        // Resource was deleted externally
        has_changes = true;
        changes.push(format!("    {} resource no longer exists", "-".red()));
    } else {
        // Check for modified, added, and removed attributes
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
                        "\u{2192}".dimmed(),
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

    if has_changes {
        *updated_count += 1;
        println!(
            "  {} \"{}\"{}:",
            id.display_type().cyan(),
            id.name,
            label_suffix,
        );
        for change in &changes {
            println!("{}", change);
        }
        println!();
    } else {
        *unchanged_count += 1;
    }

    // Update state with refreshed data
    if fresh_state.exists {
        let owned_resource;
        let res = match resource {
            Some(r) => r,
            None => {
                owned_resource = Resource::with_provider(&id.provider, &id.resource_type, &id.name);
                &owned_resource
            }
        };
        let existing_rs = state.find_resource(&id.provider, &id.resource_type, &id.name);
        let resource_state = ResourceState::from_provider_state(res, fresh_state, existing_rs)?;
        state.upsert_resource(resource_state);
    } else {
        state.remove_resource(&id.provider, &id.resource_type, &id.name);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// Load the fixture state file from `tests/fixtures/state_lookup/`.
    fn load_fixture_state() -> StateFile {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = PathBuf::from(format!(
            "{}/tests/fixtures/state_lookup/carina.state.json",
            manifest_dir
        ));
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", path.display(), e));
        serde_json::from_str(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path.display(), e))
    }

    // --- find_resource_by_query tests ---

    #[test]
    fn find_resource_by_binding() {
        let state = load_fixture_state();
        let found = find_resource_by_query(&state, "vpc").unwrap();
        assert_eq!(found.name, "my-vpc");
        assert_eq!(found.resource_type, "ec2.vpc");
    }

    #[test]
    fn find_resource_by_name_fallback() {
        let state = load_fixture_state();
        // "main-rt" has no binding, so lookup by name should work
        let found = find_resource_by_query(&state, "main-rt").unwrap();
        assert_eq!(found.resource_type, "ec2.route_table");
    }

    #[test]
    fn find_resource_not_found() {
        let state = load_fixture_state();
        assert!(find_resource_by_query(&state, "nonexistent").is_none());
    }

    #[test]
    fn binding_takes_precedence_over_name() {
        let mut state = StateFile::new();
        let mut rs1 = ResourceState::new("ec2.vpc", "vpc", "awscc");
        rs1.binding = Some("my_vpc".to_string());
        let mut rs2 = ResourceState::new("ec2.subnet", "my_vpc", "awscc");
        rs2.binding = None;
        state.upsert_resource(rs1);
        state.upsert_resource(rs2);

        let found = find_resource_by_query(&state, "my_vpc").unwrap();
        // Should find the one with binding="my_vpc", not name="my_vpc"
        assert_eq!(found.resource_type, "ec2.vpc");
    }

    // --- format_raw_value tests ---

    #[test]
    fn format_raw_value_string() {
        assert_eq!(format_raw_value(&json!("hello")), "hello");
    }

    #[test]
    fn format_raw_value_bool() {
        assert_eq!(format_raw_value(&json!(true)), "true");
    }

    #[test]
    fn format_raw_value_number() {
        assert_eq!(format_raw_value(&json!(42)), "42");
    }

    #[test]
    fn format_raw_value_null() {
        assert_eq!(format_raw_value(&json!(null)), "null");
    }

    #[test]
    fn format_raw_value_object() {
        let result = format_raw_value(&json!({"key": "value"}));
        assert!(result.contains("\"key\""));
        assert!(result.contains("\"value\""));
    }

    // --- format_state_list fixture tests ---

    #[test]
    fn state_list_shows_all_resources() {
        let state = load_fixture_state();
        let lines = format_state_list(&state);
        let output = lines.join("\n");
        insta::assert_snapshot!(output);
    }

    // --- format_state_lookup fixture tests ---

    #[test]
    fn lookup_full_resource_returns_json() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "vpc", false).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_attribute_returns_raw_value() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "vpc.vpc_id", false).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_attribute_json_returns_quoted_value() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "vpc.vpc_id", true).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_boolean_attribute_raw() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "vpc.enable_dns_support", false).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_boolean_attribute_json() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "vpc.enable_dns_support", true).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_object_attribute() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "subnet.tags", false).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_nonexistent_resource_returns_error() {
        let state = load_fixture_state();
        let err = format_state_lookup(&state, "nonexistent", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Resource 'nonexistent' not found"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn lookup_nonexistent_attribute_returns_error() {
        let state = load_fixture_state();
        let err = format_state_lookup(&state, "vpc.nonexistent_attr", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Attribute 'nonexistent_attr' not found"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn lookup_resource_without_binding_by_name() {
        let state = load_fixture_state();
        // route_table has no binding, look up by name
        let output = format_state_lookup(&state, "main-rt", false).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn lookup_resource_without_binding_attribute() {
        let state = load_fixture_state();
        let output = format_state_lookup(&state, "main-rt.route_table_id", false).unwrap();
        insta::assert_snapshot!(output);
    }

    // --- complete_state_lookup_from tests ---

    fn candidate_values(candidates: &[CompletionCandidate]) -> Vec<String> {
        let mut values: Vec<String> = candidates
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        values.sort();
        values
    }

    #[test]
    fn completion_empty_input_returns_all_resource_names() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "");
        let values = candidate_values(&candidates);
        // vpc (binding), subnet (binding), main-rt (name, no binding)
        assert_eq!(values, vec!["main-rt", "subnet", "vpc"]);
    }

    #[test]
    fn completion_partial_resource_name() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "v");
        let values = candidate_values(&candidates);
        assert_eq!(values, vec!["vpc"]);
    }

    #[test]
    fn completion_no_match() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "nonexistent");
        assert!(candidates.is_empty());
    }

    #[test]
    fn completion_attribute_names_after_dot() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "vpc.");
        let values = candidate_values(&candidates);
        assert_eq!(
            values,
            vec!["vpc.cidr_block", "vpc.enable_dns_support", "vpc.vpc_id"]
        );
    }

    #[test]
    fn completion_attribute_partial_match() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "vpc.v");
        let values = candidate_values(&candidates);
        assert_eq!(values, vec!["vpc.vpc_id"]);
    }

    #[test]
    fn completion_attribute_unknown_resource() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "unknown.");
        assert!(candidates.is_empty());
    }

    #[test]
    fn completion_resource_without_binding_by_name() {
        let state = load_fixture_state();
        let candidates = complete_state_lookup_from(&state, "main-rt.");
        let values = candidate_values(&candidates);
        assert_eq!(values, vec!["main-rt.route_table_id", "main-rt.vpc_id"]);
    }

    // --- format_state_show tests ---

    #[test]
    fn state_show_displays_all_resources_with_attributes() {
        let state = load_fixture_state();
        let output = format_state_show(&state);
        insta::assert_snapshot!(output);
    }

    // --- run_force_unlock tests ---

    #[tokio::test]
    async fn force_unlock_without_backend_uses_local_backend() {
        // When no backend block is configured, run_force_unlock should
        // fall back to create_local_backend() instead of erroring with
        // "No backend configuration found. force-unlock requires a backend."
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        // Use the state fixture which has no backend block
        let path = PathBuf::from(format!("{}/tests/fixtures/state", manifest_dir));
        let provider_context = ProviderContext::default();

        // Call force-unlock with a dummy lock ID.
        // The local backend will return LockNotFound because there is no lock file,
        // but crucially it should NOT return "No backend configuration found".
        let result = run_force_unlock("dummy-lock-id", &path, &provider_context).await;

        // Should get LockNotFound (the local backend works), not a config error
        match &result {
            Err(AppError::Config(msg)) if msg.contains("Lock with ID") => {
                // This is the expected LockNotFound error mapped to AppError::Config
            }
            Err(AppError::Config(msg)) if msg.contains("No backend configuration found") => {
                panic!(
                    "force-unlock should fall back to local backend, got: {}",
                    msg
                );
            }
            other => {
                panic!("unexpected result: {:?}", other);
            }
        }
    }

    // --- build_plan_from_state tests ---

    #[test]
    fn build_plan_from_state_creates_read_effects() {
        let state = load_fixture_state();
        let plan = build_plan_from_state(&state);

        assert_eq!(plan.effects().len(), 3);
        for effect in plan.effects() {
            assert_eq!(effect.kind(), "read");
        }
    }

    #[test]
    fn build_plan_from_state_preserves_bindings() {
        let state = load_fixture_state();
        let plan = build_plan_from_state(&state);

        let vpc_effect = &plan.effects()[0];
        assert_eq!(vpc_effect.binding_name(), Some("vpc".to_string()),);
    }

    #[test]
    fn build_plan_from_state_preserves_attributes() {
        let state = load_fixture_state();
        let plan = build_plan_from_state(&state);

        let vpc_resource = plan.effects()[0].resource().unwrap();
        assert!(vpc_resource.attributes.contains_key("cidr_block"));
        assert!(vpc_resource.attributes.contains_key("vpc_id"));
    }

    #[test]
    fn build_plan_from_state_empty() {
        let state = StateFile::new();
        let plan = build_plan_from_state(&state);
        assert!(plan.is_empty());
    }

    #[test]
    fn build_plan_from_state_preserves_dependency_bindings() {
        let state = load_fixture_state();
        let plan = build_plan_from_state(&state);

        // subnet depends on vpc
        let subnet_resource = plan.effects()[1].resource().unwrap();
        assert_eq!(subnet_resource.dependency_bindings, vec!["vpc".to_string()]);
    }
}
