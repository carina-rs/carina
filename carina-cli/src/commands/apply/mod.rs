use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use colored::Colorize;

use futures::stream::{self, StreamExt};

use carina_core::binding_index::ResolvedBindings;
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
    bindings: &mut ResolvedBindings,
    current_states: &mut HashMap<ResourceId, State>,
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> ApplyResult {
    let input = ExecutionInput {
        plan,
        unresolved_resources,
        bindings: std::mem::take(bindings),
        current_states: std::mem::take(current_states),
    };

    let observer = CliObserver::new(plan);
    let result = carina_core::executor::execute_plan(provider, input, &observer).await;

    // Write back the updated current_states so callers see refreshes
    *current_states = result.current_states.clone();

    result
}

/// Re-load each upstream declared in the saved plan and verify its
/// attribute map matches the snapshot the plan was computed against
/// (#2303). Fails on the first drifted binding so the user gets an
/// actionable message naming what changed; pretending the apply will
/// succeed and silently mixing plan-time and apply-time values
/// produces incorrect cascade re-resolution.
async fn verify_upstream_snapshot(
    sources: &[crate::commands::plan::UpstreamSource],
    snapshot: &HashMap<String, HashMap<String, Value>>,
    base_dir: &std::path::Path,
) -> Result<(), AppError> {
    use carina_core::parser::UpstreamState;
    let upstream_states: Vec<UpstreamState> = sources
        .iter()
        .map(|s| UpstreamState {
            binding: s.binding.clone(),
            source: s.source.clone(),
        })
        .collect();
    let provider_context = ProviderContext::default();
    let mut cycle_guard = super::plan::seed_cycle_guard(base_dir);
    let current = super::plan::load_upstream_states(
        &upstream_states,
        base_dir,
        &provider_context,
        &mut cycle_guard,
        super::plan::UpstreamMissingStatePolicy::Strict,
    )
    .await?;

    diff_upstream_snapshot(snapshot, &current).map_err(AppError::Config)
}

/// Pure comparison between a planned snapshot and a freshly-loaded
/// upstream view. Returns the user-facing error message naming the
/// first binding that disagrees, or `Ok(())` when the two views are
/// equal.
///
/// Split out so it can be unit-tested without a real upstream backend.
fn diff_upstream_snapshot(
    snapshot: &HashMap<String, HashMap<String, Value>>,
    current: &HashMap<String, HashMap<String, Value>>,
) -> Result<(), String> {
    for (binding, planned_attrs) in snapshot {
        match current.get(binding) {
            None => {
                return Err(format!(
                    "upstream_state '{}' was present at plan time but is missing now. \
                     Re-run 'carina plan' to capture the current upstream view.",
                    binding
                ));
            }
            Some(current_attrs) if current_attrs != planned_attrs => {
                return Err(format!(
                    "upstream_state '{}' has drifted since the plan was created. \
                     Re-run 'carina plan' so the apply uses the values it was computed against.",
                    binding
                ));
            }
            Some(_) => {}
        }
    }
    for binding in current.keys() {
        if !snapshot.contains_key(binding) {
            return Err(format!(
                "upstream_state '{}' was added since the plan was created. \
                 Re-run 'carina plan' so the apply sees the new upstream binding.",
                binding
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod upstream_snapshot_tests {
    use super::*;

    fn binding(name: &str, attrs: &[(&str, &str)]) -> (String, HashMap<String, Value>) {
        let map = attrs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect();
        (name.to_string(), map)
    }

    #[test]
    fn matching_snapshot_passes() {
        let snapshot: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let current = snapshot.clone();
        assert!(diff_upstream_snapshot(&snapshot, &current).is_ok());
    }

    #[test]
    fn drifted_attribute_fails() {
        // Same binding name, but the attribute value changed underneath.
        // This is the original bug: cascade re-resolution would silently
        // see "vpc-B" while the static plan was built around "vpc-A".
        let snapshot: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let current: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-B")])]
                .into_iter()
                .collect();
        let err = diff_upstream_snapshot(&snapshot, &current).expect_err("must fail");
        assert!(
            err.contains("network"),
            "error must name the binding: {err}"
        );
        assert!(err.contains("drifted"), "error must say drifted: {err}");
    }

    #[test]
    fn binding_disappeared_fails() {
        let snapshot: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let current: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let err = diff_upstream_snapshot(&snapshot, &current).expect_err("must fail");
        assert!(err.contains("missing now"), "got: {err}");
    }

    #[test]
    fn binding_appeared_fails() {
        let snapshot: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let current: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let err = diff_upstream_snapshot(&snapshot, &current).expect_err("must fail");
        assert!(err.contains("added since"), "got: {err}");
    }

    #[test]
    fn empty_both_sides_passes() {
        let empty: HashMap<String, HashMap<String, Value>> = HashMap::new();
        assert!(diff_upstream_snapshot(&empty, &empty).is_ok());
    }
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
        let exports = resolve_exports(input.export_params, &state)?;
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
    export_params: &[carina_core::parser::InferredExportParam],
) -> Result<(), AppError> {
    let mut state = state_file.unwrap_or_default();
    let exports = resolve_exports(export_params, &state)?;
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
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
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
                    parsed = load_configuration_with_config(
                        path,
                        provider_context,
                        &carina_core::schema::SchemaRegistry::new(),
                    )?
                    .parsed;
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
    parsed: &mut carina_core::parser::InferredFile,
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
        super::plan::UpstreamMissingStatePolicy::Strict,
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

    // Build initial bindings for reference resolution
    let mut bindings = ResolvedBindings::from_resources_with_state(
        &sorted_resources,
        &current_states,
        &remote_bindings,
    );

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
        &mut bindings,
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

    // Validate version compatibility. Plan-file version 2 added the
    // upstream-state snapshot (#2303); older plans cannot guarantee the
    // upstream values used during cascade re-resolution match what the
    // plan was computed against, so they are rejected outright per the
    // repo's no-backward-compat policy.
    if plan_file.version != 2 {
        return Err(AppError::Config(format!(
            "Unsupported plan file version: {} (expected 2). \
             Re-run 'carina plan' to produce a plan in the current format.",
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

    // Verify upstream-state bindings have not drifted since `carina
    // plan` ran (#2303). Re-load each upstream the plan declared and
    // compare against the persisted snapshot; if any binding's
    // attribute map disagrees, fail rather than silently mixing
    // plan-time and apply-time values during cascade re-resolution.
    let upstream_snapshot = plan_file.upstream_snapshot.clone();
    if !plan_file.upstream_sources.is_empty() {
        verify_upstream_snapshot(&plan_file.upstream_sources, &upstream_snapshot, base_dir).await?;
    }
    let mut bindings = ResolvedBindings::from_resources_with_state(
        sorted_resources,
        &current_states,
        &upstream_snapshot,
    );

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
        &mut bindings,
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
mod tests;
