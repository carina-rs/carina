use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use colored::Colorize;

use futures::stream::{self, StreamExt};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::effect::Effect;
use carina_core::executor::{ExecutionInput, ExecutionResult};
use carina_core::module_resolver;
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer};
use carina_core::resolver::resolve_refs_with_state_and_remote;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::value::format_value;
use carina_state::{LockInfo, ResourceState, StateBackend, StateFile, resolve_backend};

use carina_core::parser::ProviderContext;

use super::validate_and_resolve_with_config;
use crate::DetailLevel;
use crate::commands::plan::PlanFile;
use crate::commands::shared::effect_execution::{
    execute_import_effects, execute_state_only_effects,
};
use crate::commands::shared::observer::CliObserver;
#[cfg(test)]
use crate::commands::shared::progress::format_duration;
use crate::commands::shared::progress::{
    RefreshProgress, emit_newline_on_interrupt, refresh_multi_progress,
};
use crate::commands::shared::state_writeback::{
    ApplyStateSave, FinalizeApplyInput, apply_name_overrides, build_state_after_apply,
    resolve_exports,
};
use crate::commands::state::map_lock_error;
use crate::display::print_plan;
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, create_providers_from_configs,
    get_provider_with_ctx, read_data_source_with_retry, read_with_retry,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
    resolve_data_source_refs_for_refresh, resolve_names_with_ctx,
};

/// Re-export ExecutionResult as the public API for apply results.
pub type ApplyResult = ExecutionResult;

/// Execute all effects in a plan, resolving references dynamically.
///
/// This delegates to `carina_core::executor::execute_plan()` with a `CliObserver`
/// for colored progress output.
pub async fn execute_effects(
    plan: &Plan,
    provider: &dyn Provider,
    binding_map: &mut HashMap<String, HashMap<String, Value>>,
    current_states: &mut HashMap<ResourceId, State>,
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> ApplyResult {
    let input = ExecutionInput {
        plan,
        unresolved_resources,
        binding_map: std::mem::take(binding_map),
        current_states: std::mem::take(current_states),
    };

    let observer = CliObserver::new(plan);
    let result = carina_core::executor::execute_plan(provider, input, &observer).await;

    // Write back the updated current_states so callers see refreshes
    *current_states = result.current_states.clone();

    result
}

/// Refresh states for resources whose operations failed.
///
/// This is kept for use by tests in `tests.rs`. The core executor has its own
/// internal version.
#[cfg(test)]
pub async fn refresh_pending_states(
    provider: &dyn Provider,
    current_states: &mut HashMap<ResourceId, State>,
    pending_refreshes: &HashMap<ResourceId, String>,
) -> HashSet<ResourceId> {
    if pending_refreshes.is_empty() {
        return HashSet::new();
    }

    println!();
    println!("{}", "Refreshing uncertain resource states...".cyan());

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by_key(|(left_id, _)| left_id.to_string());
    let mut failed_refreshes = HashSet::new();

    for (id, identifier) in refreshes {
        match read_with_retry(provider, id, Some(identifier)).await {
            Ok(state) => {
                println!("  {} Refresh {}", "✓".green(), id);
                current_states.insert(id.clone(), state);
            }
            Err(error) => {
                println!("  {} Refresh {} - {}", "!".yellow(), id, error);
                failed_refreshes.insert(id.clone());
            }
        }
    }

    failed_refreshes
}

/// Save state after apply. Does NOT release the lock -- caller is responsible.
///
/// When `lock` is `None` (i.e. `--lock=false`), state is written without lock
/// validation via `save_state_unlocked`.
pub(crate) async fn finalize_apply(input: FinalizeApplyInput<'_>) -> Result<(), AppError> {
    println!();
    println!("{}", "Saving state...".cyan());

    let mut state = build_state_after_apply(ApplyStateSave {
        state_file: input.state_file,
        sorted_resources: input.sorted_resources,
        current_states: input.current_states,
        applied_states: &input.result.applied_states,
        permanent_name_overrides: &input.result.permanent_name_overrides,
        plan: input.plan,
        successfully_deleted: &input.result.successfully_deleted,
        failed_refreshes: &input.result.failed_refreshes,
        schemas: input.schemas,
    })?;

    // Resolve exports and persist to state
    if !input.export_params.is_empty() {
        let exports = resolve_exports(input.export_params, &state);
        state.exports = exports;
    }

    if let Some(lock) = input.lock {
        save_state_locked(input.backend, lock, &mut state).await?;
    } else {
        save_state_unlocked(input.backend, &mut state).await?;
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    Ok(())
}

/// Renew the lock and write state with lock validation.
///
/// This ensures that the lock is still held before writing state, preventing
/// silent state corruption when a lock has expired and been acquired by another
/// process during a long-running operation.
pub async fn save_state_locked(
    backend: &dyn StateBackend,
    lock: &LockInfo,
    state: &mut StateFile,
) -> Result<(), AppError> {
    let renewed = backend.renew_lock(lock).await.map_err(AppError::Backend)?;
    state.increment_serial();
    backend
        .write_state_locked(state, &renewed)
        .await
        .map_err(AppError::Backend)
}

/// Write state without lock validation.
///
/// Used when `--lock=false` is specified. Increments the serial number and
/// writes using `write_state` (no lock check).
pub async fn save_state_unlocked(
    backend: &dyn StateBackend,
    state: &mut StateFile,
) -> Result<(), AppError> {
    state.increment_serial();
    backend.write_state(state).await.map_err(AppError::Backend)
}

/// Persist export changes when the resource plan is empty.
///
/// Used when `plan.is_empty()` short-circuits apply: resources don't need
/// any work, but exports may have changed. Rebuild the exports from the
/// current state + desired `export_params` and write the state.
pub(crate) async fn persist_exports_only(
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    state_file: Option<StateFile>,
    export_params: &[carina_core::parser::ExportParameter],
) -> Result<(), AppError> {
    let mut state = state_file.unwrap_or_default();
    let exports = resolve_exports(export_params, &state);
    state.exports = exports;
    if let Some(lk) = lock {
        save_state_locked(backend, lk, &mut state).await?;
    } else {
        save_state_unlocked(backend, &mut state).await?;
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);
    println!("  {} Exports updated", "✓".green());
    Ok(())
}

/// Detect infrastructure drift by comparing planned states against actual infrastructure.
///
/// Returns `Ok(None)` if no drift is detected, or `Ok(Some(messages))` with drift details.
/// Returns `Err` if a resource is missing from planned_states or if a provider read fails.
pub async fn detect_drift(
    sorted_resources: &[Resource],
    planned_states: &HashMap<ResourceId, State>,
    provider: &dyn Provider,
) -> Result<Option<Vec<String>>, AppError> {
    let mut drift_detected = false;
    let mut drift_messages: Vec<String> = Vec::new();

    for resource in sorted_resources {
        // Skip virtual resources (module attribute containers)
        if resource.is_virtual() {
            continue;
        }

        let planned_state = planned_states.get(&resource.id);
        let identifier = planned_state.and_then(|s| s.identifier.as_deref());

        let actual_state = provider
            .read(&resource.id, identifier)
            .await
            .map_err(AppError::Provider)?;

        if let Some(planned) = planned_state {
            if planned.exists != actual_state.exists {
                drift_detected = true;
                if planned.exists {
                    drift_messages.push(format!(
                        "  {} {}: resource existed at plan time but no longer exists",
                        "~".yellow(),
                        resource.id
                    ));
                } else {
                    drift_messages.push(format!(
                        "  {} {}: resource did not exist at plan time but now exists",
                        "~".yellow(),
                        resource.id
                    ));
                }
            } else if planned.exists && actual_state.exists {
                // Compare attributes for existing resources
                let mut attr_diffs: Vec<String> = Vec::new();
                for (key, planned_val) in &planned.attributes {
                    if key.starts_with('_') {
                        continue;
                    }
                    match actual_state.attributes.get(key) {
                        Some(actual_val) if actual_val != planned_val => {
                            attr_diffs.push(format!(
                                "      {}: {} → {}",
                                key,
                                format_value(planned_val),
                                format_value(actual_val)
                            ));
                        }
                        None => {
                            attr_diffs.push(format!(
                                "      {}: {} → (removed)",
                                key,
                                format_value(planned_val)
                            ));
                        }
                        _ => {}
                    }
                }
                for (key, actual_val) in &actual_state.attributes {
                    if key.starts_with('_') {
                        continue;
                    }
                    if !planned.attributes.contains_key(key) {
                        attr_diffs.push(format!(
                            "      {}: (none) → {}",
                            key,
                            format_value(actual_val)
                        ));
                    }
                }
                if !attr_diffs.is_empty() {
                    drift_detected = true;
                    drift_messages.push(format!(
                        "  {} {}: attributes have changed since plan was created:",
                        "~".yellow(),
                        resource.id
                    ));
                    drift_messages.extend(attr_diffs);
                }
            }
        } else {
            return Err(AppError::Config(format!(
                "Resource {} is present in plan but missing from planned states. \
                 The plan file may be corrupted. Please re-run 'carina plan'.",
                resource.id
            )));
        }
    }

    if drift_detected {
        Ok(Some(drift_messages))
    } else {
        Ok(None)
    }
}

pub async fn run_apply(
    path: &PathBuf,
    auto_approve: bool,
    lock: bool,
    reconfigure: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(path, provider_context)?;
    let mut parsed = loaded.parsed;
    let backend_file = loaded.backend_file;

    let base_dir = get_base_dir(path);
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);
    validate_and_resolve_with_config(&mut parsed, base_dir, false)?;

    // Detect backend reconfiguration before touching any state
    crate::commands::check_backend_lock(base_dir, parsed.backend.as_ref(), reconfigure)?;

    // Check for backend configuration - use local backend by default
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = resolve_backend(backend_config)
        .await
        .map_err(AppError::Backend)?;

    // Handle bootstrap if S3 backend is configured
    #[allow(unused_assignments)]
    let mut lock_info: Option<LockInfo> = None;

    if let Some(config) = backend_config {
        // Check if bucket exists (bootstrap detection)
        let bucket_exists = backend.bucket_exists().await.map_err(AppError::Backend)?;

        if !bucket_exists {
            println!(
                "{}",
                "State bucket not found. Running bootstrap..."
                    .yellow()
                    .bold()
            );

            // Get bucket name from config
            let bucket_name = config
                .attributes
                .get("bucket")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or("Missing bucket name in backend configuration")?;

            // Check if there's a bucket resource defined with matching name
            let backend_resource_type = backend
                .resource_type()
                .ok_or("Backend does not specify a resource type")?;
            if let Some(bucket_resource) =
                parsed.find_resource_by_attr(backend_resource_type, "bucket", &bucket_name)
            {
                println!("Found state bucket resource in configuration.");
                println!(
                    "Creating bucket '{}' before other resources...",
                    bucket_name.cyan()
                );

                // Create the bucket resource using the factory pattern
                let backend_provider_name = backend
                    .provider_name()
                    .ok_or("Backend does not specify a provider name")?;
                let factory = provider_mod::find_factory(ctx.factories(), backend_provider_name)
                    .ok_or_else(|| {
                        format!("No provider factory found for '{}'", backend_provider_name)
                    })?;
                let provider_config_attrs = parsed
                    .providers
                    .iter()
                    .find(|p| p.name == backend_provider_name)
                    .map(|p| p.attributes.clone())
                    .unwrap_or_default();
                let bucket_provider = factory.create_provider(&provider_config_attrs).await;

                match bucket_provider.create(bucket_resource).await {
                    Ok(_) => {
                        println!("  {} Created state bucket: {}", "✓".green(), bucket_name);
                    }
                    Err(e) => {
                        return Err(AppError::Config(format!(
                            "Failed to create state bucket: {}",
                            e
                        )));
                    }
                }
            } else {
                // Auto-create the bucket if auto_create is enabled
                let auto_create = config
                    .attributes
                    .get("auto_create")
                    .and_then(|v| match v {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(true);

                if auto_create {
                    println!("Auto-creating state bucket: {}", bucket_name.cyan());
                    backend.create_bucket().await.map_err(AppError::Backend)?;
                    println!("  {} Created state bucket", "✓".green());

                    let backend_provider_name = backend
                        .provider_name()
                        .ok_or("Backend does not specify a provider name")?;

                    // Append resource definition to backend file
                    let target_file = backend_file.clone().unwrap_or_else(|| path.clone());

                    let resource_code = backend
                        .resource_definition(&bucket_name)
                        .ok_or("Backend does not support resource definition generation")?;

                    // Read existing content if file exists, then append
                    let mut content = if target_file.exists() {
                        fs::read_to_string(&target_file).map_err(|e| {
                            format!("Failed to read {}: {}", target_file.display(), e)
                        })?
                    } else {
                        String::new()
                    };
                    content.push_str(&resource_code);

                    fs::write(&target_file, &content)
                        .map_err(|e| format!("Failed to write {}: {}", target_file.display(), e))?;
                    println!(
                        "  {} Added resource definition to {}",
                        "✓".green(),
                        target_file.display()
                    );

                    // Create a protected ResourceState for the auto-created bucket
                    let backend_resource_type = backend
                        .resource_type()
                        .ok_or("Backend does not specify a resource type")?;
                    let bucket_state = ResourceState::new(
                        backend_resource_type,
                        &bucket_name,
                        backend_provider_name,
                    )
                    .with_attribute("bucket".to_string(), serde_json::json!(bucket_name))
                    .with_attribute(
                        "versioning_status".to_string(),
                        serde_json::json!("Enabled"),
                    )
                    .with_protected(true);

                    // Initialize state with the protected bucket
                    let mut initial_state = StateFile::new();
                    initial_state.upsert_resource(bucket_state);
                    backend
                        .write_state(&initial_state)
                        .await
                        .map_err(AppError::Backend)?;
                    println!(
                        "  {} Registered state bucket as protected resource",
                        "✓".green()
                    );

                    // Re-parse the updated configuration to include the new resource
                    parsed = load_configuration_with_config(path, provider_context)?.parsed;
                    if let Err(e) = module_resolver::resolve_modules_with_config(
                        &mut parsed,
                        get_base_dir(path),
                        provider_context,
                    ) {
                        return Err(AppError::Config(format!("Module resolution error: {}", e)));
                    }
                    let name_errors = resolve_names_with_ctx(&ctx, &mut parsed.resources);
                    if !name_errors.is_empty() {
                        return Err(super::collapse_errors(name_errors));
                    }
                } else {
                    return Err(AppError::Config(format!(
                        "Backend bucket '{}' not found and auto_create is disabled",
                        bucket_name
                    )));
                }
            }

            // Initialize state if not already done (when bucket existed or was created from resource)
            if backend
                .read_state()
                .await
                .map_err(AppError::Backend)?
                .is_none()
            {
                backend.init().await.map_err(AppError::Backend)?;
            }
        }
    }

    // Acquire lock (unless --lock=false)
    if lock {
        println!("{}", "Acquiring state lock...".cyan());
        lock_info = Some(
            backend
                .acquire_lock("apply")
                .await
                .map_err(map_lock_error)?,
        );
        println!("  {} Lock acquired", "✓".green());
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow()
                .bold()
        );
    }

    // All code after lock acquisition is wrapped so that lock release is guaranteed.
    // Ctrl+C cancels the operation and returns Interrupted so the lock is still released.
    let op_result = crate::signal::run_with_ctrl_c(run_apply_locked(
        &ctx,
        &mut parsed,
        auto_approve,
        backend.as_ref(),
        lock_info.as_ref(),
        base_dir,
        provider_context,
    ))
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = backend.release_lock(li).await.map_err(AppError::Backend);

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

async fn run_apply_locked(
    ctx: &WiringContext,
    parsed: &mut carina_core::parser::ParsedFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    base_dir: &std::path::Path,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    // Read current state from backend
    let state_file = backend.read_state().await.map_err(AppError::Backend)?;

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    if let Some(sf) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.name.clone())
                    .collect()
            },
        );
        reconcile_anonymous_identifiers_with_ctx(ctx, &mut parsed.resources, sf);
    }
    apply_name_overrides(&mut parsed.resources, &state_file);

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(ctx, parsed, base_dir).await;

    // Upstream state bindings are loaded up front so refs that target
    // `upstream_state` blocks can be resolved during refresh (#1683).
    let mut cycle_guard = super::plan::seed_cycle_guard(base_dir);
    let remote_bindings = super::plan::load_upstream_states(
        &parsed.upstream_states,
        base_dir,
        provider_context,
        &mut cycle_guard,
    )
    .await?;

    // Expand deferred for-expressions now that remote values are available.
    // Must happen BEFORE sort_resources_by_dependencies so expanded resources
    // are included in the sorted set used for planning (#1844).
    parsed.expand_deferred_for_expressions(&remote_bindings);

    // Print warnings after expansion (resolved ones are removed)
    parsed.print_warnings();

    // Sort resources by dependencies (after expansion so expanded resources are included)
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;

    // Build state-file-derived maps up front so anonymous → let-bound
    // rename transfer (#1685) can run between refresh phases 1 and 2.
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    let mut prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
        .unwrap_or_default();

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    // Skip virtual resources (module attribute containers) — they have no infrastructure.
    RefreshProgress::start_header();
    let multi = refresh_multi_progress();
    let provider_ref = &provider;
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    // Pre-build dependency_bindings from state file so we can restore them
    // after refresh. Provider.read() doesn't know about this metadata (#1565).
    let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
        .as_ref()
        .map(|sf| {
            sorted_resources
                .iter()
                .filter_map(|r| {
                    let rs =
                        sf.find_resource(&r.id.provider, &r.id.resource_type, r.id.name_str())?;
                    if rs.dependency_bindings.is_empty() {
                        None
                    } else {
                        Some((r.id.clone(), rs.dependency_bindings.clone()))
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Phase 1: refresh managed (non-data-source) resources in parallel.
    let phase1_results: Vec<Result<(ResourceId, State), AppError>> = stream::iter(
        sorted_resources
            .iter()
            .filter(|r| !r.is_virtual() && !r.is_data_source()),
    )
    .map(|resource| {
        let progress = RefreshProgress::begin_multi(&multi, &resource.id);
        let identifier = state_file
            .as_ref()
            .and_then(|sf| sf.get_identifier_for_resource(resource));
        let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
        async move {
            let mut state = read_with_retry(provider_ref, &resource.id, identifier.as_deref())
                .await
                .map_err(AppError::Provider)?;
            if let Some(deps) = dep_bindings {
                state.dependency_bindings = deps;
            }
            progress.finish();
            Ok((resource.id.clone(), state))
        }
    })
    .buffer_unordered(5)
    .collect()
    .await;
    for result in phase1_results {
        let (id, state) = result?;
        current_states.insert(id, state);
    }

    // Refresh orphaned resources (#844, #1685). Must run before the
    // rename transfer below so old-name entries are present for
    // `apply_anonymous_to_named_renames` to transfer.
    let mut orphan_dependencies: HashMap<ResourceId, BTreeSet<String>> = HashMap::new();
    if let Some(sf) = state_file.as_ref() {
        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        let orphan_states: Vec<(ResourceId, State)> =
            sf.build_orphan_states(&desired_ids).into_iter().collect();
        let orphan_results: Vec<Result<(ResourceId, State), AppError>> =
            stream::iter(orphan_states)
                .map(|(id, state)| {
                    let binding = state.attributes.get("_binding").cloned();
                    let dep_bindings = state.dependency_bindings.clone();
                    async move {
                        let mut refreshed =
                            read_with_retry(provider_ref, &id, state.identifier.as_deref())
                                .await
                                .map_err(AppError::Provider)?;
                        if let Some(b) = binding {
                            refreshed.attributes.insert("_binding".to_string(), b);
                        }
                        if !dep_bindings.is_empty() {
                            refreshed.dependency_bindings = dep_bindings;
                        }
                        Ok((id, refreshed))
                    }
                })
                .buffer_unordered(5)
                .collect()
                .await;
        for result in orphan_results {
            let (id, refreshed) = result?;
            if refreshed.exists {
                current_states.entry(id).or_insert(refreshed);
            }
        }
        orphan_dependencies = sf.build_orphan_dependencies(&desired_ids);
    }

    // Hydrate, transfer state for moved blocks and anonymous → let-bound
    // renames (#1685), then run phase 2 against the consolidated state.
    provider.hydrate_read_state(&mut current_states, &saved_attrs);
    let moved_pairs = {
        let mut pairs = crate::wiring::materialize_moved_states(
            &mut current_states,
            &mut prev_desired_keys,
            &mut saved_attrs,
            &parsed.state_blocks,
            &state_file,
        );
        pairs.extend(crate::wiring::apply_anonymous_to_named_renames(
            ctx,
            &sorted_resources,
            &parsed.providers,
            &mut current_states,
            &mut prev_desired_keys,
            &mut saved_attrs,
            &state_file,
        ));
        pairs
    };

    // Phase 2: resolve data source inputs against the consolidated state
    // and refresh them via `read_data_source` (#1683, #1685).
    let resolved_data_sources =
        resolve_data_source_refs_for_refresh(&sorted_resources, &current_states, &remote_bindings)?;
    let phase2_results: Vec<Result<(ResourceId, State), AppError>> =
        stream::iter(resolved_data_sources.iter())
            .map(|resource| {
                let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
                async move {
                    let mut state = read_data_source_with_retry(provider_ref, resource)
                        .await
                        .map_err(AppError::Provider)?;
                    if let Some(deps) = dep_bindings {
                        state.dependency_bindings = deps;
                    }
                    progress.finish();
                    Ok((resource.id.clone(), state))
                }
            })
            .buffer_unordered(5)
            .collect()
            .await;
    for result in phase2_results {
        let (id, state) = result?;
        current_states.insert(id, state);
    }

    // Build initial binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &sorted_resources {
        if let Some(ref binding_name) = resource.binding {
            let mut attrs: HashMap<String, Value> = resource.resolved_attributes();
            // Merge existing state if available
            if let Some(state) = current_states.get(&resource.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    if !attrs.contains_key(k) {
                        attrs.insert(k.clone(), v.clone());
                    }
                }
            }
            binding_map.insert(binding_name.clone(), attrs);
        }
    }

    // Resolve references and enum identifiers, then create initial plan for display
    let mut resources_for_plan = sorted_resources.clone();
    resolve_refs_with_state_and_remote(&mut resources_for_plan, &current_states, &remote_bindings)?;

    // Run the normalization pipeline (same as plan path in wiring.rs).
    let preprocessor = crate::wiring::PlanPreprocessor::new(&provider, ctx);
    preprocessor.prepare(
        &mut resources_for_plan,
        &mut current_states,
        &parsed.providers,
    );

    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();
    let schemas = ctx.schemas();
    let mut plan = create_plan(
        &resources_for_plan,
        &current_states,
        &lifecycles,
        schemas,
        &saved_attrs,
        &prev_desired_keys,
        &orphan_dependencies,
    );

    // Populate cascading updates for create_before_destroy Replace effects.
    // Uses unresolved resources (sorted_resources) so dependents retain ResourceRef values.
    cascade_dependent_updates(&mut plan, &sorted_resources, &current_states, schemas);

    // Add state block effects (import/removed/moved) to the plan
    crate::wiring::add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        &state_file,
        &moved_pairs,
        schemas,
    );

    // Check for prevent_destroy violations
    if plan.has_errors() {
        for err in plan.errors() {
            eprintln!("{} {}", "Error:".red().bold(), err);
        }
        return Err(AppError::Validation(format!(
            "{} resource(s) have prevent_destroy set and cannot be deleted or replaced",
            plan.errors().len()
        )));
    }

    if plan.is_empty() {
        // Even when no resources need changes, exports may have changed.
        // Persist them before returning so state stays in sync with config.
        let resolved_exports = crate::commands::plan::resolve_export_values_for_display(
            &parsed.export_params,
            &sorted_resources,
            &current_states,
        );
        let empty_exports = HashMap::new();
        let current_exports = state_file
            .as_ref()
            .map(|s| &s.exports)
            .unwrap_or(&empty_exports);
        let export_changes =
            crate::commands::plan::compute_export_diffs(&resolved_exports, current_exports);

        if export_changes.is_empty() {
            println!("{}", "No changes needed.".green());
            return Ok(());
        }

        print_plan(
            &plan,
            DetailLevel::Full,
            &HashMap::new(),
            Some(ctx.schemas()),
            &HashMap::new(),
            &export_changes,
            &parsed.deferred_for_expressions,
        );

        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let interrupt = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        if confirm_apply(stdin, interrupt, auto_approve).await? == ApplyConfirmation::Cancelled {
            return Ok(());
        }

        println!(
            "{}",
            format!(
                "Persisting {} export change(s) to state.",
                export_changes.len()
            )
            .cyan()
        );
        persist_exports_only(backend, lock, state_file, &parsed.export_params).await?;
        return Ok(());
    }

    // Build delete attributes map from current states for display
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();

    let resolved_exports = crate::commands::plan::resolve_export_values_for_display(
        &parsed.export_params,
        &sorted_resources,
        &current_states,
    );
    let current_exports = state_file
        .as_ref()
        .map(|s| s.exports.clone())
        .unwrap_or_default();
    let export_changes =
        crate::commands::plan::compute_export_diffs(&resolved_exports, &current_exports);
    print_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(ctx.schemas()),
        &moved_origins,
        &export_changes,
        &parsed.deferred_for_expressions,
    );

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if confirm_apply(stdin, interrupt, auto_approve).await? == ApplyConfirmation::Cancelled {
        return Ok(());
    }

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    // Build unresolved resource map for re-resolution at apply time
    let unresolved_resources: HashMap<ResourceId, Resource> = sorted_resources
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();

    let mut result = execute_effects(
        &plan,
        &provider,
        &mut binding_map,
        &mut current_states,
        &unresolved_resources,
    )
    .await;

    // Execute import effects: read imported resources from the provider
    execute_import_effects(&plan, &provider, &mut result).await;

    // Execute remove and move effects (state-only, logged for user feedback)
    execute_state_only_effects(&plan, &mut result);

    finalize_apply(FinalizeApplyInput {
        result: &result,
        state_file,
        sorted_resources: &sorted_resources,
        current_states: &current_states,
        plan: &plan,
        backend,
        lock,
        schemas,
        export_params: &parsed.export_params,
    })
    .await?;

    println!();
    if result.failure_count == 0 && result.skip_count == 0 {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", result.success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        let mut parts = vec![format!("{} succeeded", result.success_count)];
        if result.failure_count > 0 {
            parts.push(format!("{} failed", result.failure_count));
        }
        if result.skip_count > 0 {
            parts.push(format!("{} skipped", result.skip_count));
        }
        Err(AppError::Config(format!(
            "Apply failed. {}.",
            parts.join(", ")
        )))
    }
}

pub async fn run_apply_from_plan(
    plan_path: &PathBuf,
    auto_approve: bool,
    lock: bool,
) -> Result<(), AppError> {
    // Read and deserialize the plan file
    let content =
        fs::read_to_string(plan_path).map_err(|e| format!("Failed to read plan file: {}", e))?;
    let plan_file: PlanFile =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse plan file: {}", e))?;

    // Validate version compatibility
    if plan_file.version != 1 {
        return Err(AppError::Config(format!(
            "Unsupported plan file version: {} (expected 1)",
            plan_file.version
        )));
    }

    let current_version = env!("CARGO_PKG_VERSION");
    if plan_file.carina_version != current_version {
        println!(
            "{}",
            format!(
                "Warning: plan was created with carina {} but current version is {}",
                plan_file.carina_version, current_version
            )
            .yellow()
        );
    }

    println!(
        "{}",
        format!(
            "Using saved plan from {} (created {})",
            plan_file.source_path, plan_file.timestamp
        )
        .cyan()
    );

    // Set up backend
    let backend: Box<dyn StateBackend> = resolve_backend(plan_file.backend_config.as_ref())
        .await
        .map_err(AppError::Backend)?;

    // Acquire lock (unless --lock=false)
    let lock_info: Option<LockInfo> = if lock {
        println!("{}", "Acquiring state lock...".cyan());
        let li = backend
            .acquire_lock("apply")
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

    let source_path = std::path::PathBuf::from(&plan_file.source_path);
    let base_dir = get_base_dir(&source_path);
    let op_result = crate::signal::run_with_ctrl_c(run_apply_from_plan_locked(
        plan_file,
        auto_approve,
        backend.as_ref(),
        lock_info.as_ref(),
        base_dir,
    ))
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = backend.release_lock(li).await.map_err(AppError::Backend);

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

async fn run_apply_from_plan_locked(
    plan_file: PlanFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    base_dir: &std::path::Path,
) -> Result<(), AppError> {
    // Read current state and validate lineage
    let state_file = backend.read_state().await.map_err(AppError::Backend)?;

    if let Some(ref state) = state_file {
        // Validate state lineage
        if let Some(ref plan_lineage) = plan_file.state_lineage
            && &state.lineage != plan_lineage
        {
            return Err(AppError::Config(format!(
                "State lineage mismatch: plan was created for lineage '{}' but current state has '{}'",
                plan_lineage, state.lineage
            )));
        }

        // Warn on serial mismatch (state may have drifted)
        if let Some(plan_serial) = plan_file.state_serial
            && state.serial != plan_serial
        {
            println!(
                "{}",
                format!(
                    "Warning: state serial has changed since plan was created ({} → {}). \
                     The infrastructure may have drifted.",
                    plan_serial, state.serial
                )
                .yellow()
            );
        }
    }

    let plan = &plan_file.plan;
    let sorted_resources = &plan_file.sorted_resources;

    // Rebuild planned current_states HashMap from plan file
    let planned_states: HashMap<ResourceId, State> = plan_file
        .current_states
        .into_iter()
        .map(|entry| (entry.id, entry.state))
        .collect();

    // Create provider early for drift detection
    let provider = create_providers_from_configs(&plan_file.provider_configs, base_dir).await;

    // Drift detection: re-read actual infrastructure state and compare against planned states
    println!("{}", "Checking for infrastructure drift...".cyan());
    let drift_result = detect_drift(sorted_resources, &planned_states, &provider).await?;

    if let Some(drift_messages) = drift_result {
        println!();
        println!("{}", "Error: Infrastructure drift detected!".red().bold());
        println!(
            "{}",
            "The following resources have changed since the plan was created:".red()
        );
        println!();
        for msg in &drift_messages {
            println!("{}", msg);
        }
        println!();
        println!(
            "{}",
            "Please re-run 'carina plan' to create a new plan that reflects the current state."
                .yellow()
        );
        return Err(AppError::Config(
            "Apply aborted due to infrastructure drift.".to_string(),
        ));
    }

    println!("  {} No drift detected.", "✓".green());

    // Use the actual states (freshly read) as current_states for apply
    let mut current_states = planned_states;

    // Check for prevent_destroy violations
    if plan.has_errors() {
        for err in plan.errors() {
            eprintln!("{} {}", "Error:".red().bold(), err);
        }
        return Err(AppError::Validation(format!(
            "{} resource(s) have prevent_destroy set and cannot be deleted or replaced",
            plan.errors().len()
        )));
    }

    if plan.is_empty() {
        println!("{}", "No changes needed.".green());
        return Ok(());
    }

    // Build delete attributes map from current states for display
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();

    print_plan(
        plan,
        DetailLevel::Full,
        &delete_attributes,
        None,
        &HashMap::new(),
        &[],
        &[],
    );

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if confirm_apply(stdin, interrupt, auto_approve).await? == ApplyConfirmation::Cancelled {
        return Ok(());
    }

    // Build initial binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in sorted_resources {
        if let Some(ref binding_name) = resource.binding {
            let mut attrs: HashMap<String, Value> = resource.resolved_attributes();
            if let Some(state) = current_states.get(&resource.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    if !attrs.contains_key(k) {
                        attrs.insert(k.clone(), v.clone());
                    }
                }
            }
            binding_map.insert(binding_name.clone(), attrs);
        }
    }

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    // Build unresolved resource map for re-resolution at apply time
    let unresolved_resources: HashMap<ResourceId, Resource> = sorted_resources
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();

    let mut result = execute_effects(
        plan,
        &provider,
        &mut binding_map,
        &mut current_states,
        &unresolved_resources,
    )
    .await;

    // Execute import effects: read imported resources from the provider
    execute_import_effects(plan, &provider, &mut result).await;

    // Execute remove and move effects (state-only, logged for user feedback)
    execute_state_only_effects(plan, &mut result);

    // Build schemas for write-only attribute persistence
    let (factories, _) = build_factories_from_providers(&plan_file.provider_configs, base_dir);
    let ctx = WiringContext::new(factories);

    finalize_apply(FinalizeApplyInput {
        result: &result,
        state_file,
        sorted_resources,
        current_states: &current_states,
        plan,
        backend,
        lock,
        schemas: ctx.schemas(),
        export_params: &[],
    })
    .await?;

    println!();
    if result.failure_count == 0 && result.skip_count == 0 {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", result.success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        let mut parts = vec![format!("{} succeeded", result.success_count)];
        if result.failure_count > 0 {
            parts.push(format!("{} failed", result.failure_count));
        }
        if result.skip_count > 0 {
            parts.push(format!("{} skipped", result.skip_count));
        }
        Err(AppError::Config(format!(
            "Apply failed. {}.",
            parts.join(", ")
        )))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyConfirmation {
    Confirmed,
    Cancelled,
}

/// Prompt the user to confirm an apply. Shared between the resource-change and
/// export-only paths so both use identical wording and behavior.
pub(crate) async fn confirm_apply<R, F>(
    reader: R,
    interrupt: F,
    auto_approve: bool,
) -> Result<ApplyConfirmation, AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
    F: std::future::Future<Output = ()>,
{
    if auto_approve {
        return Ok(ApplyConfirmation::Confirmed);
    }

    println!(
        "{}",
        "Do you want to perform these actions?".yellow().bold()
    );
    println!(
        "  {}",
        "Carina will perform the actions described above. Type 'yes' to confirm.".yellow()
    );
    print!("\n  Enter a value: ");
    std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

    let read_result = crate::signal::read_line_with_interrupt(reader, interrupt).await;
    emit_newline_on_interrupt(&mut std::io::stdout(), &read_result);
    let input = read_result?;

    if input.trim() != "yes" {
        println!();
        println!("{}", "Apply cancelled.".yellow());
        Ok(ApplyConfirmation::Cancelled)
    } else {
        println!();
        Ok(ApplyConfirmation::Confirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use std::time::Duration;

    #[test]
    fn build_state_after_apply_finds_write_only_with_provider_prefix() {
        // The schema map is keyed by provider-prefixed names (e.g., "awscc.ec2.Vpc"),
        // but the buggy code used resource.id.resource_type (e.g., "ec2.Vpc") for lookup.
        // This test verifies that write-only attributes are found when the schema key
        // includes the provider prefix.
        let mut schemas = HashMap::new();
        let schema = ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
            .attribute(
                AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only(),
            );
        // Schema is registered with provider-prefixed key
        schemas.insert("awscc.ec2.Vpc".to_string(), schema);

        let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        resource.set_attr(
            "ipv4_netmask_length".to_string(),
            Value::String("16".to_string()),
        );

        let sorted_resources = vec![resource];

        // Simulate provider returning state without the write-only attribute
        let mut applied_attrs = HashMap::new();
        applied_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs);
        let mut applied_states = HashMap::new();
        applied_states.insert(sorted_resources[0].id.clone(), applied_state);

        let current_states = HashMap::new();
        let permanent_name_overrides = HashMap::new();
        let plan = Plan::new();
        let successfully_deleted = HashSet::new();
        let failed_refreshes = HashSet::new();

        let result = build_state_after_apply(ApplyStateSave {
            state_file: None,
            sorted_resources: &sorted_resources,
            current_states: &current_states,
            applied_states: &applied_states,
            permanent_name_overrides: &permanent_name_overrides,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        })
        .unwrap();

        // The write-only attribute should be merged from the desired resource into state
        let saved = result
            .find_resource("awscc", "ec2.Vpc", "my-vpc")
            .expect("resource should exist in state");
        assert_eq!(
            saved.attributes.get("ipv4_netmask_length"),
            Some(&serde_json::Value::String("16".to_string())),
            "write-only attribute should be persisted in state"
        );
    }

    #[test]
    fn build_state_after_apply_preserves_block_name_attribute() {
        // When a block_name attribute (e.g., "policies" with block_name "policy")
        // is carried over by the provider because CloudControl doesn't return it,
        // the state after apply should include the attribute under the canonical name.
        // This is the scenario in issue #1499 (iam_role/with_policy).
        use carina_core::schema::StructField;

        let mut schemas = HashMap::new();
        let schema = ResourceSchema::new("iam.role")
            .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new(
                    "policies",
                    AttributeType::unordered_list(AttributeType::Struct {
                        name: "Policy".to_string(),
                        fields: vec![
                            StructField::new("policy_name", AttributeType::String).required(),
                            StructField::new("policy_document", AttributeType::String).required(),
                        ],
                    }),
                )
                .with_block_name("policy"),
            );
        schemas.insert("awscc.iam.role".to_string(), schema);

        // Resource with resolved block name (policy -> policies)
        let mut resource = Resource::with_provider("awscc", "iam.role", "test-role");
        resource.set_attr(
            "role_name".to_string(),
            Value::String("test-role".to_string()),
        );
        resource.set_attr(
            "policies".to_string(),
            Value::List(vec![Value::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::String("test-policy".to_string()),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::String("{}".to_string()),
                    ),
                ]
                .into_iter()
                .collect(),
            )]),
        );

        let sorted_resources = vec![resource];

        // Simulate provider returning state WITH carried-over policies attribute
        // (This is what AwsccProvider::create_resource does in the carry-over logic)
        let mut applied_attrs = HashMap::new();
        applied_attrs.insert(
            "role_name".to_string(),
            Value::String("test-role".to_string()),
        );
        applied_attrs.insert(
            "policies".to_string(),
            Value::List(vec![Value::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::String("test-policy".to_string()),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::String("{}".to_string()),
                    ),
                ]
                .into_iter()
                .collect(),
            )]),
        );
        let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs)
            .with_identifier("some-identifier");
        let mut applied_states = HashMap::new();
        applied_states.insert(sorted_resources[0].id.clone(), applied_state);

        let current_states = HashMap::new();
        let permanent_name_overrides = HashMap::new();
        let plan = Plan::new();
        let successfully_deleted = HashSet::new();
        let failed_refreshes = HashSet::new();

        let state = build_state_after_apply(ApplyStateSave {
            state_file: None,
            sorted_resources: &sorted_resources,
            current_states: &current_states,
            applied_states: &applied_states,
            permanent_name_overrides: &permanent_name_overrides,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        })
        .unwrap();

        // Verify state has the policies attribute
        let saved = state
            .find_resource("awscc", "iam.role", "test-role")
            .expect("resource should exist in state");
        assert!(
            saved.attributes.contains_key("policies"),
            "state should contain 'policies' attribute (carried over from desired)"
        );

        // Verify desired_keys includes "policies" (canonical name, not "policy")
        assert!(
            saved.desired_keys.contains(&"policies".to_string()),
            "desired_keys should contain 'policies': {:?}",
            saved.desired_keys
        );

        // Now simulate second plan: build_saved_attrs should return the policies
        let saved_attrs = state.build_saved_attrs();
        let id = carina_core::resource::ResourceId::with_provider("awscc", "iam.role", "test-role");
        let attrs = saved_attrs.get(&id).unwrap();
        assert!(
            attrs.contains_key("policies"),
            "saved_attrs should contain 'policies': {:?}",
            attrs.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn block_name_attribute_no_diff_when_hydrated() {
        // After apply, the state file contains the block_name attribute (canonical name).
        // On re-plan, if hydrate_read_state restores it into current_states,
        // the differ should see no changes.
        //
        // This tests the scenario from issue #1499 where plan-verify fails
        // because the block_name attribute shows as an addition.
        use carina_core::differ::diff;
        use carina_core::schema::StructField;

        let schema = ResourceSchema::new("awscc.iam.role")
            .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new(
                    "policies",
                    AttributeType::unordered_list(AttributeType::Struct {
                        name: "Policy".to_string(),
                        fields: vec![
                            StructField::new("policy_name", AttributeType::String).required(),
                            StructField::new("policy_document", AttributeType::String).required(),
                        ],
                    }),
                )
                .with_block_name("policy"),
            );

        // Desired resource (after resolve_block_names: "policy" -> "policies")
        let mut resource = Resource::with_provider("awscc", "iam.role", "test-role");
        resource.set_attr(
            "role_name".to_string(),
            Value::String("test-role".to_string()),
        );
        resource.set_attr(
            "policies".to_string(),
            Value::List(vec![Value::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::String("test-policy".to_string()),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::String("{}".to_string()),
                    ),
                ]
                .into_iter()
                .collect(),
            )]),
        );

        // Current state: simulate hydration restoring the policies attribute
        let mut state_attrs = HashMap::new();
        state_attrs.insert(
            "role_name".to_string(),
            Value::String("test-role".to_string()),
        );
        state_attrs.insert(
            "policies".to_string(),
            Value::List(vec![Value::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::String("test-policy".to_string()),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::String("{}".to_string()),
                    ),
                ]
                .into_iter()
                .collect(),
            )]),
        );
        let current = State::existing(resource.id.clone(), state_attrs).with_identifier("some-id");

        // Saved attrs: same as current (from previous apply)
        let saved: HashMap<String, Value> = current.attributes.clone();

        // Previous desired keys: what was in the resource on first apply
        let prev_desired_keys = vec!["policies".to_string(), "role_name".to_string()];

        let d = diff(
            &resource,
            &current,
            Some(&saved),
            Some(&prev_desired_keys),
            Some(&schema),
        );

        assert!(
            matches!(d, carina_core::differ::Diff::NoChange(_)),
            "Expected no change, but got: {:?}",
            d
        );
    }

    #[test]
    fn block_name_attribute_state_roundtrip() {
        // Verify that block_name attributes (saved under canonical name in state)
        // roundtrip correctly through state save/load, meaning the saved_attrs
        // returned by build_saved_attrs have the correct canonical key.
        //
        // This covers the ec2_ipam case (operating_region -> operating_regions)
        // from issue #1499.
        use carina_core::schema::StructField;

        let mut schemas = HashMap::new();
        let schema = ResourceSchema::new("ec2.ipam")
            .attribute(
                AttributeSchema::new(
                    "operating_regions",
                    AttributeType::unordered_list(AttributeType::Struct {
                        name: "IpamOperatingRegion".to_string(),
                        fields: vec![
                            StructField::new("region_name", AttributeType::String).required(),
                        ],
                    }),
                )
                .with_block_name("operating_region"),
            )
            .attribute(AttributeSchema::new("description", AttributeType::String));
        schemas.insert("awscc.ec2.ipam".to_string(), schema);

        // Resource with resolved block name
        let mut resource = Resource::with_provider("awscc", "ec2.ipam", "test-ipam");
        resource.set_attr(
            "operating_regions".to_string(),
            Value::List(vec![Value::Map(
                vec![(
                    "region_name".to_string(),
                    Value::String("ap-northeast-1".to_string()),
                )]
                .into_iter()
                .collect(),
            )]),
        );
        resource.set_attr(
            "description".to_string(),
            Value::String("test IPAM".to_string()),
        );

        let sorted_resources = vec![resource];

        // Simulate provider state with carried-over operating_regions
        let mut applied_attrs = HashMap::new();
        applied_attrs.insert(
            "description".to_string(),
            Value::String("test IPAM".to_string()),
        );
        applied_attrs.insert(
            "operating_regions".to_string(),
            Value::List(vec![Value::Map(
                vec![(
                    "region_name".to_string(),
                    Value::String("ap-northeast-1".to_string()),
                )]
                .into_iter()
                .collect(),
            )]),
        );
        let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs)
            .with_identifier("ipam-12345");
        let mut applied_states = HashMap::new();
        applied_states.insert(sorted_resources[0].id.clone(), applied_state);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: None,
            sorted_resources: &sorted_resources,
            current_states: &HashMap::new(),
            applied_states: &applied_states,
            permanent_name_overrides: &HashMap::new(),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        })
        .unwrap();

        // Verify state contains operating_regions
        let saved_rs = state
            .find_resource("awscc", "ec2.ipam", "test-ipam")
            .expect("resource should exist");
        assert!(
            saved_rs.attributes.contains_key("operating_regions"),
            "state should contain 'operating_regions'"
        );
        assert!(
            saved_rs
                .desired_keys
                .contains(&"operating_regions".to_string()),
            "desired_keys should contain 'operating_regions'"
        );

        // Verify roundtrip through saved_attrs
        let saved_attrs = state.build_saved_attrs();
        let id = carina_core::resource::ResourceId::with_provider("awscc", "ec2.ipam", "test-ipam");
        let attrs = saved_attrs.get(&id).unwrap();
        let operating_regions = attrs
            .get("operating_regions")
            .expect("should have operating_regions");

        // Verify the value structure is preserved
        if let Value::List(items) = operating_regions {
            assert_eq!(items.len(), 1);
            if let Value::Map(map) = &items[0] {
                assert_eq!(
                    map.get("region_name"),
                    Some(&Value::String("ap-northeast-1".to_string()))
                );
            } else {
                panic!("Expected Map in list, got {:?}", items[0]);
            }
        } else {
            panic!("Expected List, got {:?}", operating_regions);
        }
    }

    #[test]
    fn format_duration_sub_second() {
        let d = Duration::from_millis(500);
        assert_eq!(format_duration(d), "0.5s");
    }

    #[test]
    fn format_duration_seconds() {
        let d = Duration::from_secs_f64(3.25);
        assert_eq!(format_duration(d), "3.2s");
    }

    #[test]
    fn format_duration_minutes() {
        let d = Duration::from_secs_f64(65.3);
        assert_eq!(format_duration(d), "1m 5.3s");
    }

    #[test]
    fn format_duration_zero() {
        let d = Duration::from_secs(0);
        assert_eq!(format_duration(d), "0.0s");
    }

    #[test]
    fn resolve_exports_resolves_cross_file_dot_notation_strings() {
        use carina_core::parser::ExportParameter;
        use carina_core::resource::Value;
        use carina_state::StateFile;

        // Build a state file with a resource that has a binding and attributes
        let state = {
            let json = serde_json::json!({
                "version": 5,
                "serial": 1,
                "lineage": "test",
                "carina_version": "0.4.0",
                "resources": [
                    {
                        "resource_type": "organizations.account",
                        "name": "registry-prod",
                        "identifier": "459524413166",
                        "provider": "awscc",
                        "binding": "registry_prod",
                        "attributes": {
                            "account_id": "459524413166",
                            "account_name": "registry-prod"
                        }
                    }
                ]
            });
            serde_json::from_value::<StateFile>(json).unwrap()
        };

        // Export param references registry_prod.account_id as a dot-notation string
        // (this is how cross-file references are parsed: exports.crn doesn't see
        // the let binding in main.crn, so the parser emits a plain string)
        let export_params = vec![ExportParameter {
            name: "account_id".to_string(),
            type_expr: None,
            value: Some(Value::String("registry_prod.account_id".to_string())),
        }];

        let exports = resolve_exports(&export_params, &state);

        assert_eq!(
            exports.get("account_id"),
            Some(&serde_json::Value::String("459524413166".to_string())),
            "resolve_exports should resolve dot-notation strings to actual values. Got: {:?}",
            exports
        );
    }

    #[test]
    fn emit_newline_on_interrupt_writes_newline_when_interrupted() {
        let mut buf: Vec<u8> = Vec::new();
        let result: Result<String, AppError> = Err(AppError::Interrupted);
        emit_newline_on_interrupt(&mut buf, &result);
        assert_eq!(buf, b"\n");
    }

    #[test]
    fn emit_newline_on_interrupt_writes_nothing_on_ok() {
        let mut buf: Vec<u8> = Vec::new();
        let result: Result<String, AppError> = Ok("yes".to_string());
        emit_newline_on_interrupt(&mut buf, &result);
        assert!(buf.is_empty());
    }

    #[test]
    fn emit_newline_on_interrupt_writes_nothing_on_other_error() {
        let mut buf: Vec<u8> = Vec::new();
        let result: Result<String, AppError> = Err(AppError::Config("boom".to_string()));
        emit_newline_on_interrupt(&mut buf, &result);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn confirm_apply_returns_confirmed_on_yes() {
        let input = &b"yes\n"[..];
        let interrupt = std::future::pending::<()>();
        let outcome = confirm_apply(input, interrupt, false).await.unwrap();
        assert_eq!(outcome, ApplyConfirmation::Confirmed);
    }

    #[tokio::test]
    async fn confirm_apply_returns_cancelled_on_no() {
        let input = &b"no\n"[..];
        let interrupt = std::future::pending::<()>();
        let outcome = confirm_apply(input, interrupt, false).await.unwrap();
        assert_eq!(outcome, ApplyConfirmation::Cancelled);
    }

    #[tokio::test]
    async fn confirm_apply_returns_cancelled_on_empty_input() {
        let input = &b"\n"[..];
        let interrupt = std::future::pending::<()>();
        let outcome = confirm_apply(input, interrupt, false).await.unwrap();
        assert_eq!(outcome, ApplyConfirmation::Cancelled);
    }

    #[tokio::test]
    async fn confirm_apply_auto_approve_skips_read() {
        // Reader would hang forever; auto_approve must short-circuit without reading.
        let input = tokio::io::BufReader::new(tokio::io::empty());
        let interrupt = std::future::pending::<()>();
        let outcome = confirm_apply(input, interrupt, true).await.unwrap();
        assert_eq!(outcome, ApplyConfirmation::Confirmed);
    }

    #[tokio::test]
    async fn confirm_apply_propagates_interrupt() {
        // A reader that never resolves, to force the interrupt path.
        struct NeverReady;
        impl tokio::io::AsyncRead for NeverReady {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Pending
            }
        }
        let reader = tokio::io::BufReader::new(NeverReady);
        let interrupt = async {};
        let err = confirm_apply(reader, interrupt, false).await.unwrap_err();
        assert!(matches!(err, AppError::Interrupted));
    }
}
