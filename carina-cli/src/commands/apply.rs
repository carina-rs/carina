use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::effect::Effect;
use carina_core::module_resolver;
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer};
use carina_core::resolver::{resolve_ref_value, resolve_refs_with_state};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::value::format_value;
use carina_state::{
    BackendConfig as StateBackendConfig, LockInfo, ResourceState, StateBackend, StateFile,
    create_backend, create_local_backend,
};

use super::validate_and_resolve;
use crate::commands::plan::PlanFile;
use crate::commands::state::map_lock_error;
use crate::display::{format_effect, print_plan};
use crate::error::AppError;
use crate::wiring::{
    WiringContext, create_providers_from_configs, get_provider_with_ctx,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names, resolve_names_with_ctx,
};

/// Apply permanent name overrides from state to desired resources.
///
/// When a create_before_destroy replacement produces a non-renameable temporary name
/// (can_rename=false), the state stores the permanent name. This function applies
/// those overrides so the plan doesn't detect a false diff.
pub fn apply_name_overrides(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    let overrides = state_file.build_name_overrides();
    if overrides.is_empty() {
        return;
    }

    for resource in resources.iter_mut() {
        if let Some(name_overrides) = overrides.get(&resource.id) {
            for (attr, value) in name_overrides {
                resource
                    .attributes
                    .insert(attr.clone(), Value::String(value.clone()));
            }
        }
    }
}

/// Result of executing a plan's effects.
pub struct ApplyResult {
    pub success_count: usize,
    pub failure_count: usize,
    pub skip_count: usize,
    pub applied_states: HashMap<ResourceId, State>,
    pub successfully_deleted: HashSet<ResourceId>,
    pub permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub failed_refreshes: HashSet<ResourceId>,
}

/// Execute all effects in a plan, resolving references dynamically.
///
/// This is the shared apply execution logic used by both `run_apply()` and
/// `run_apply_from_plan()`.
pub async fn execute_effects(
    plan: &Plan,
    provider: &dyn Provider,
    binding_map: &mut HashMap<String, HashMap<String, Value>>,
    current_states: &mut HashMap<ResourceId, State>,
    unresolved_resources: &HashMap<ResourceId, Resource>,
) -> ApplyResult {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();
    let mut permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>> = HashMap::new();
    let mut pending_refreshes: HashMap<ResourceId, String> = HashMap::new();

    for effect in plan.effects() {
        // Check if any dependency has failed - skip this effect if so
        if let Some(failed_dep) =
            carina_core::deps::find_failed_dependency(effect, &failed_bindings)
        {
            println!(
                "  {} {} - dependency '{}' failed",
                "⊘".yellow(),
                format_effect(effect),
                failed_dep
            );
            skip_count += 1;
            // Propagate failure to this binding so transitive dependents are also skipped
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
            continue;
        }

        match effect {
            Effect::Create(resource) => {
                // Re-resolve references with current binding_map
                let mut resolved_resource = resource.clone();
                for (key, value) in &resource.attributes {
                    resolved_resource
                        .attributes
                        .insert(key.clone(), resolve_ref_value(value, binding_map));
                }

                match provider.create(&resolved_resource).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;

                        // Track the applied state
                        applied_states.insert(resource.id.clone(), state.clone());

                        // Update binding_map with the newly created resource's state (including id)
                        if let Some(Value::String(binding_name)) =
                            resource.attributes.get("_binding")
                        {
                            let mut attrs = resolved_resource.attributes.clone();
                            for (k, v) in &state.attributes {
                                attrs.insert(k.clone(), v.clone());
                            }
                            binding_map.insert(binding_name.clone(), attrs);
                        }
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Update { id, from, to, .. } => {
                // Re-resolve references from unresolved resource if available.
                // The `to` in the effect may contain stale pre-resolved values when
                // a dependency was replaced via create_before_destroy. Using the
                // unresolved resource's attributes (which still contain ResourceRef
                // values) ensures we resolve against the updated binding_map.
                let resolve_source = unresolved_resources.get(id).unwrap_or(to);
                let mut resolved_to = to.clone();
                for (key, value) in &resolve_source.attributes {
                    resolved_to
                        .attributes
                        .insert(key.clone(), resolve_ref_value(value, binding_map));
                }

                // Get identifier from current state
                let identifier = from.identifier.as_deref().unwrap_or("");
                match provider.update(id, identifier, from, &resolved_to).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;

                        // Track the applied state
                        applied_states.insert(id.clone(), state.clone());

                        // Update binding_map
                        if let Some(Value::String(binding_name)) = to.attributes.get("_binding") {
                            let mut attrs = resolved_to.attributes.clone();
                            for (k, v) in &state.attributes {
                                attrs.insert(k.clone(), v.clone());
                            }
                            binding_map.insert(binding_name.clone(), attrs);
                        }
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                        queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Replace {
                id,
                from,
                to,
                lifecycle,
                cascading_updates,
                temporary_name,
                ..
            } => {
                if lifecycle.create_before_destroy {
                    // Create the new resource first
                    let mut resolved_resource = to.clone();
                    for (key, value) in &to.attributes {
                        resolved_resource
                            .attributes
                            .insert(key.clone(), resolve_ref_value(value, binding_map));
                    }

                    match provider.create(&resolved_resource).await {
                        Ok(state) => {
                            // Update binding_map with the new resource's state before cascading
                            if let Some(Value::String(binding_name)) = to.attributes.get("_binding")
                            {
                                let mut attrs = resolved_resource.attributes.clone();
                                for (k, v) in &state.attributes {
                                    attrs.insert(k.clone(), v.clone());
                                }
                                binding_map.insert(binding_name.clone(), attrs);
                            }

                            // Execute cascading updates for dependent resources
                            let mut cascade_failed = false;
                            for cascade in cascading_updates {
                                let mut resolved_to = cascade.to.clone();
                                for (key, value) in &cascade.to.attributes {
                                    resolved_to
                                        .attributes
                                        .insert(key.clone(), resolve_ref_value(value, binding_map));
                                }
                                let cascade_identifier =
                                    cascade.from.identifier.as_deref().unwrap_or("");
                                match provider
                                    .update(
                                        &cascade.id,
                                        cascade_identifier,
                                        &cascade.from,
                                        &resolved_to,
                                    )
                                    .await
                                {
                                    Ok(cascade_state) => {
                                        println!(
                                            "  {} Update {} (cascade)",
                                            "✓".green(),
                                            cascade.id
                                        );
                                        applied_states
                                            .insert(cascade.id.clone(), cascade_state.clone());
                                        if let Some(Value::String(binding_name)) =
                                            cascade.to.attributes.get("_binding")
                                        {
                                            let mut attrs = resolved_to.attributes.clone();
                                            for (k, v) in &cascade_state.attributes {
                                                attrs.insert(k.clone(), v.clone());
                                            }
                                            binding_map.insert(binding_name.clone(), attrs);
                                        }
                                    }
                                    Err(e) => {
                                        println!(
                                            "  {} Update {} (cascade) - {}",
                                            "✗".red(),
                                            cascade.id,
                                            e
                                        );
                                        queue_state_refresh(
                                            &mut pending_refreshes,
                                            &cascade.id,
                                            Some(cascade_identifier),
                                        );
                                        cascade_failed = true;
                                        failure_count += 1;
                                        break;
                                    }
                                }
                            }

                            if cascade_failed {
                                queue_state_refresh(
                                    &mut pending_refreshes,
                                    &to.id,
                                    state.identifier.as_deref(),
                                );
                                // Don't delete old resource if cascade failed
                                if let Some(binding) = effect.binding_name() {
                                    failed_bindings.insert(binding);
                                }
                            } else {
                                // Then delete the old resource
                                let identifier = from.identifier.as_deref().unwrap_or("");
                                match provider.delete(id, identifier, lifecycle).await {
                                    Ok(()) => {
                                        // If a temporary name was used and the name is updatable,
                                        // rename the resource back to the desired name
                                        let mut rename_failed = false;
                                        let final_state = if let Some(temp) = temporary_name
                                            && temp.can_rename
                                        {
                                            let new_identifier =
                                                state.identifier.as_deref().unwrap_or("");
                                            let mut rename_to = to.clone();
                                            rename_to.attributes.insert(
                                                temp.attribute.clone(),
                                                Value::String(temp.original_value.clone()),
                                            );
                                            match provider
                                                .update(id, new_identifier, &state, &rename_to)
                                                .await
                                            {
                                                Ok(renamed_state) => {
                                                    println!(
                                                        "  {} Rename {} \"{}\" → \"{}\"",
                                                        "✓".green(),
                                                        id,
                                                        temp.temporary_value,
                                                        temp.original_value
                                                    );
                                                    renamed_state
                                                }
                                                Err(e) => {
                                                    println!(
                                                        "  {} Rename {} - {}",
                                                        "✗".red(),
                                                        id,
                                                        e
                                                    );
                                                    rename_failed = true;
                                                    // Use the state with temp name
                                                    state.clone()
                                                }
                                            }
                                        } else {
                                            // Track permanent name override for can_rename=false
                                            if let Some(temp) = temporary_name
                                                && !temp.can_rename
                                            {
                                                let mut overrides = HashMap::new();
                                                overrides.insert(
                                                    temp.attribute.clone(),
                                                    temp.temporary_value.clone(),
                                                );
                                                permanent_name_overrides
                                                    .insert(to.id.clone(), overrides);
                                            }
                                            state.clone()
                                        };

                                        // Save state regardless (resource exists, possibly with temp name)
                                        applied_states.insert(to.id.clone(), final_state);

                                        if rename_failed {
                                            println!(
                                                "  {} {} (rename failed)",
                                                "✗".red(),
                                                format_effect(effect)
                                            );
                                            failure_count += 1;
                                            if let Some(binding) = effect.binding_name() {
                                                failed_bindings.insert(binding);
                                            }
                                        } else {
                                            println!("  {} {}", "✓".green(), format_effect(effect));
                                            success_count += 1;
                                        }
                                    }
                                    Err(e) => {
                                        println!(
                                            "  {} {} - {}",
                                            "✗".red(),
                                            format_effect(effect),
                                            e
                                        );
                                        failure_count += 1;
                                        queue_state_refresh(
                                            &mut pending_refreshes,
                                            &to.id,
                                            state.identifier.as_deref(),
                                        );
                                        if let Some(binding) = effect.binding_name() {
                                            failed_bindings.insert(binding);
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                            failure_count += 1;
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        }
                    }
                } else {
                    // Delete the existing resource first
                    let identifier = from.identifier.as_deref().unwrap_or("");
                    match provider.delete(id, identifier, lifecycle).await {
                        Ok(()) => {
                            // Re-resolve references with current binding_map
                            let mut resolved_resource = to.clone();
                            for (key, value) in &to.attributes {
                                resolved_resource
                                    .attributes
                                    .insert(key.clone(), resolve_ref_value(value, binding_map));
                            }

                            // Create the new resource
                            match provider.create(&resolved_resource).await {
                                Ok(state) => {
                                    println!("  {} {}", "✓".green(), format_effect(effect));
                                    success_count += 1;

                                    applied_states.insert(to.id.clone(), state.clone());

                                    if let Some(Value::String(binding_name)) =
                                        to.attributes.get("_binding")
                                    {
                                        let mut attrs = resolved_resource.attributes.clone();
                                        for (k, v) in &state.attributes {
                                            attrs.insert(k.clone(), v.clone());
                                        }
                                        binding_map.insert(binding_name.clone(), attrs);
                                    }
                                }
                                Err(e) => {
                                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                                    failure_count += 1;
                                    queue_state_refresh(
                                        &mut pending_refreshes,
                                        &to.id,
                                        Some(identifier),
                                    );
                                    if let Some(binding) = effect.binding_name() {
                                        failed_bindings.insert(binding);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                            failure_count += 1;
                            queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        }
                    }
                }
            }
            Effect::Delete {
                id,
                identifier,
                lifecycle,
                ..
            } => match provider.delete(id, identifier, lifecycle).await {
                Ok(()) => {
                    println!("  {} {}", "✓".green(), format_effect(effect));
                    success_count += 1;
                    successfully_deleted.insert(id.clone());
                }
                Err(e) => {
                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                    failure_count += 1;
                    queue_state_refresh(&mut pending_refreshes, id, Some(identifier));
                }
            },
            Effect::Read { .. } => {}
        }
    }

    let failed_refreshes =
        refresh_pending_states(provider, current_states, &pending_refreshes).await;

    ApplyResult {
        success_count,
        failure_count,
        skip_count,
        applied_states,
        successfully_deleted,
        permanent_name_overrides,
        failed_refreshes,
    }
}

/// Save state after apply. Does NOT release the lock -- caller is responsible.
///
/// When `lock` is `None` (i.e. `--lock=false`), state is written without lock
/// validation via `save_state_unlocked`.
pub async fn finalize_apply(
    result: &ApplyResult,
    state_file: Option<StateFile>,
    sorted_resources: &[Resource],
    current_states: &HashMap<ResourceId, State>,
    plan: &Plan,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
) -> Result<(), AppError> {
    println!();
    println!("{}", "Saving state...".cyan());

    let mut state = build_state_after_apply(ApplyStateSave {
        state_file,
        sorted_resources,
        current_states,
        applied_states: &result.applied_states,
        permanent_name_overrides: &result.permanent_name_overrides,
        plan,
        successfully_deleted: &result.successfully_deleted,
        failed_refreshes: &result.failed_refreshes,
    })?;

    if let Some(lock) = lock {
        save_state_locked(backend, lock, &mut state).await?;
    } else {
        save_state_unlocked(backend, &mut state).await?;
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

pub fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
    }
}

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
    refreshes.sort_by(|(left_id, _), (right_id, _)| left_id.to_string().cmp(&right_id.to_string()));
    let mut failed_refreshes = HashSet::new();

    for (id, identifier) in refreshes {
        match provider.read(id, Some(identifier)).await {
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

pub struct ApplyStateSave<'a> {
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub applied_states: &'a HashMap<ResourceId, State>,
    pub permanent_name_overrides: &'a HashMap<ResourceId, HashMap<String, String>>,
    pub plan: &'a Plan,
    pub successfully_deleted: &'a HashSet<ResourceId>,
    pub failed_refreshes: &'a HashSet<ResourceId>,
}

pub fn build_state_after_apply(save: ApplyStateSave<'_>) -> Result<StateFile, AppError> {
    let ApplyStateSave {
        state_file,
        sorted_resources,
        current_states,
        applied_states,
        permanent_name_overrides,
        plan,
        successfully_deleted,
        failed_refreshes,
    } = save;
    let mut state = state_file.unwrap_or_default();

    for resource in sorted_resources {
        let existing = state.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            &resource.id.name,
        );
        if let Some(applied_state) = applied_states.get(&resource.id) {
            let mut resource_state =
                ResourceState::from_provider_state(resource, applied_state, existing)?;
            if let Some(overrides) = permanent_name_overrides.get(&resource.id) {
                resource_state.name_overrides = overrides.clone();
            }
            state.upsert_resource(resource_state);
        } else if failed_refreshes.contains(&resource.id) {
            continue;
        } else if let Some(current_state) = current_states.get(&resource.id) {
            if current_state.exists {
                let resource_state =
                    ResourceState::from_provider_state(resource, current_state, existing)?;
                state.upsert_resource(resource_state);
            } else {
                state.remove_resource(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    &resource.id.name,
                );
            }
        }
    }

    for effect in plan.effects() {
        if let Effect::Delete { id, .. } = effect
            && successfully_deleted.contains(id)
        {
            state.remove_resource(&id.provider, &id.resource_type, &id.name);
        }
    }

    Ok(state)
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

pub async fn run_apply(path: &PathBuf, auto_approve: bool, lock: bool) -> Result<(), AppError> {
    let ctx = WiringContext::new();
    let loaded = load_configuration(path)?;
    let mut parsed = loaded.parsed;
    let backend_file = loaded.backend_file;

    let base_dir = get_base_dir(path);
    validate_and_resolve(&mut parsed, base_dir, false)?;

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
                    parsed = load_configuration(path)?.parsed;
                    if let Err(e) =
                        module_resolver::resolve_modules(&mut parsed, get_base_dir(path))
                    {
                        return Err(AppError::Config(format!("Module resolution error: {}", e)));
                    }
                    resolve_names_with_ctx(&ctx, &mut parsed.resources)?;
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

    // All code after lock acquisition is wrapped so that lock release is guaranteed
    let op_result = run_apply_locked(
        &ctx,
        &mut parsed,
        auto_approve,
        backend.as_ref(),
        lock_info.as_ref(),
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

async fn run_apply_locked(
    ctx: &WiringContext,
    parsed: &mut carina_core::parser::ParsedFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
) -> Result<(), AppError> {
    // Read current state from backend
    let state_file = backend.read_state().await.map_err(AppError::Backend)?;

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    if let Some(sf) = state_file.as_ref() {
        reconcile_anonymous_identifiers_with_ctx(ctx, &mut parsed.resources, sf);
    }
    apply_name_overrides(&mut parsed.resources, &state_file);

    // Sort resources by dependencies
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(ctx, parsed).await;

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        let identifier = state_file
            .as_ref()
            .and_then(|sf| sf.get_identifier_for_resource(resource));
        let state = provider
            .read(&resource.id, identifier.as_deref())
            .await
            .map_err(AppError::Provider)?;
        current_states.insert(resource.id.clone(), state);
    }

    // Seed current_states with orphaned resources from state file (#844).
    // These are resources tracked in state but removed from the .crn config.
    if let Some(sf) = state_file.as_ref() {
        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        for (id, state) in sf.build_orphan_states(&desired_ids) {
            current_states.entry(id).or_insert(state);
        }
    }

    // Restore unreturned attributes from state file (CloudControl doesn't always return them)
    let saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    provider.hydrate_read_state(&mut current_states, &saved_attrs);

    // Build initial binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &sorted_resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            let mut attrs = resource.attributes.clone();
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
    resolve_refs_with_state(&mut resources_for_plan, &current_states);
    provider.normalize_desired(&mut resources_for_plan);
    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();
    let schemas = ctx.schemas();
    let prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
        .unwrap_or_default();
    let mut plan = create_plan(
        &resources_for_plan,
        &current_states,
        &lifecycles,
        schemas,
        &saved_attrs,
        &prev_desired_keys,
    );

    // Populate cascading updates for create_before_destroy Replace effects.
    // Uses unresolved resources (sorted_resources) so dependents retain ResourceRef values.
    cascade_dependent_updates(&mut plan, &sorted_resources, &current_states, schemas);

    if plan.is_empty() {
        println!("{}", "No changes needed.".green());
        return Ok(());
    }

    print_plan(&plan, false);

    // Confirmation prompt
    if !auto_approve {
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

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != "yes" {
            println!();
            println!("{}", "Apply cancelled.".yellow());
            return Ok(());
        }
        println!();
    }

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    // Build unresolved resource map for re-resolution at apply time
    let unresolved_resources: HashMap<ResourceId, Resource> = sorted_resources
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();

    let result = execute_effects(
        &plan,
        &provider,
        &mut binding_map,
        &mut current_states,
        &unresolved_resources,
    )
    .await;

    finalize_apply(
        &result,
        state_file,
        &sorted_resources,
        &current_states,
        &plan,
        backend,
        lock,
    )
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
    let backend: Box<dyn StateBackend> = if let Some(config) = plan_file.backend_config.as_ref() {
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

    let op_result = run_apply_from_plan_locked(
        plan_file,
        auto_approve,
        backend.as_ref(),
        lock_info.as_ref(),
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

async fn run_apply_from_plan_locked(
    plan_file: PlanFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
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
    let provider = create_providers_from_configs(&plan_file.provider_configs).await;

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

    if plan.is_empty() {
        println!("{}", "No changes needed.".green());
        return Ok(());
    }

    print_plan(plan, false);

    // Confirmation prompt
    if !auto_approve {
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

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != "yes" {
            println!();
            println!("{}", "Apply cancelled.".yellow());
            return Ok(());
        }
        println!();
    }

    // Build initial binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in sorted_resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            let mut attrs = resource.attributes.clone();
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

    let result = execute_effects(
        plan,
        &provider,
        &mut binding_map,
        &mut current_states,
        &unresolved_resources,
    )
    .await;

    finalize_apply(
        &result,
        state_file,
        sorted_resources,
        &current_states,
        plan,
        backend,
        lock,
    )
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
