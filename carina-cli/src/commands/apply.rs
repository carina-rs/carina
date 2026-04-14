use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use futures::stream::{self, StreamExt};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::effect::Effect;
use carina_core::executor::{
    ExecutionEvent, ExecutionInput, ExecutionObserver, ExecutionResult, ProgressInfo,
};
use carina_core::module_resolver;
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer};
use carina_core::resolver::resolve_refs_with_state_and_remote;
use carina_core::resource::{Expr, Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;
use carina_core::value::format_value;
use carina_state::{LockInfo, ResourceState, StateBackend, StateFile, resolve_backend};

use carina_core::parser::ProviderContext;

use super::validate_and_resolve_with_config;
use crate::DetailLevel;
use crate::commands::plan::PlanFile;
use crate::commands::state::map_lock_error;
use crate::display::{format_effect, print_plan};
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, create_providers_from_configs,
    get_provider_with_ctx, read_data_source_with_retry, read_with_retry,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
    resolve_data_source_refs_for_refresh, resolve_names_with_ctx,
};

/// Format a duration as a human-readable string like "3.2s" or "1m 5.3s".
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        let mins = secs as u64 / 60;
        let remaining = secs - (mins as f64 * 60.0);
        format!("{}m {:.1}s", mins, remaining)
    }
}

/// Braille spinner frames for animated progress display.
pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Create the spinner style used by both apply and destroy.
pub(crate) fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {msg}...")
        .unwrap()
        .tick_strings(SPINNER_FRAMES)
}

/// Spinner for tracking state refresh progress per resource.
///
/// Shows a spinner while each resource is being read, then displays timing
/// when done. Uses `indicatif` for animated terminal output with a shared
/// `MultiProgress` to support concurrent spinners.
pub(crate) struct RefreshProgress {
    pb: ProgressBar,
    start: std::time::Instant,
}

impl RefreshProgress {
    /// Print the "Refreshing state..." header and prepare for per-resource spinners.
    pub fn start_header() {
        println!("{}", "Refreshing state...".cyan());
    }

    /// Begin tracking a resource read under a shared `MultiProgress`.
    pub fn begin_multi(multi: &MultiProgress, id: &ResourceId) -> Self {
        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(spinner_style());
        pb.set_message(format!("{}", id));
        pb.enable_steady_tick(Duration::from_millis(80));
        Self {
            pb,
            start: std::time::Instant::now(),
        }
    }

    /// Finish the spinner with a success checkmark and elapsed time.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let timing = format!("[{}]", format_duration(elapsed)).dimmed();
        let msg = format!("{} {} {}", "✓".green(), self.pb.message(), timing);
        self.pb
            .set_style(ProgressStyle::with_template("  {msg}").unwrap());
        self.pb.finish_with_message(msg);
    }
}

/// Create a `MultiProgress` for concurrent refresh spinners.
///
/// Redirects the draw target to stderr when stdout is not a terminal (e.g., in CI),
/// so that spinner animations are suppressed but `println` messages still appear.
pub(crate) fn refresh_multi_progress() -> MultiProgress {
    let multi = MultiProgress::new();
    if !std::io::stdout().is_terminal() {
        multi.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    }
    multi
}

/// CLI observer that prints colored progress output using `indicatif`.
///
/// Uses dynamic display: resources appear only when they start executing or
/// complete. No upfront tree is shown. Spinners are created lazily on
/// `EffectStarted` and finished on `EffectSucceeded`/`EffectFailed`.
struct CliObserver {
    multi: MultiProgress,
    /// Map from effect description to its ProgressBar, created lazily when
    /// the effect starts executing. Guarded by a Mutex for concurrent access.
    bars: Mutex<HashMap<String, ProgressBar>>,
}

impl CliObserver {
    /// Create a new observer.
    fn new(_plan: &Plan) -> Self {
        let multi = MultiProgress::new();
        if !std::io::stdout().is_terminal() {
            multi.set_draw_target(indicatif::ProgressDrawTarget::stderr());
        }

        Self {
            multi,
            bars: Mutex::new(HashMap::new()),
        }
    }
}

/// Format a progress counter as a dimmed string like "1/10".
fn format_progress(progress: &ProgressInfo) -> String {
    format!("{}/{}", progress.completed, progress.total)
}

impl ExecutionObserver for CliObserver {
    fn on_event(&self, event: &ExecutionEvent) {
        match event {
            ExecutionEvent::Waiting { .. } => {
                // Dynamic display: don't show waiting resources.
                // They will appear when they start executing.
            }
            ExecutionEvent::EffectStarted { effect } => {
                let key = format_effect(effect);
                let pb = self.multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style());
                pb.set_message(key.clone());
                pb.enable_steady_tick(Duration::from_millis(80));
                self.bars.lock().unwrap().insert(key, pb);
            }
            ExecutionEvent::EffectSucceeded {
                effect,
                duration,
                progress,
                ..
            } => {
                let key = format_effect(effect);
                let timing = format!("[{}]", format_duration(*duration)).dimmed();
                let counter = format_progress(progress).dimmed();
                let msg = format!(
                    "{} {} {} {}",
                    "✓".green(),
                    format_effect(effect),
                    timing,
                    counter
                );
                let mut bars = self.bars.lock().unwrap();
                if let Some(pb) = bars.remove(&key) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg);
                } else {
                    eprintln!("  {msg}");
                }
            }
            ExecutionEvent::EffectFailed {
                effect,
                error,
                duration,
                progress,
            } => {
                let key = format_effect(effect);
                let timing = format!("[{}]", format_duration(*duration)).dimmed();
                let counter = format_progress(progress).dimmed();
                let msg = format!(
                    "{} {} {} {}\n      {} {}",
                    "✗".red(),
                    format_effect(effect),
                    timing,
                    counter,
                    "→".red(),
                    error.red()
                );
                let mut bars = self.bars.lock().unwrap();
                if let Some(pb) = bars.remove(&key) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg.clone());
                }
                // Always print errors to stderr so they're visible even when
                // indicatif's MultiProgress swallows progress bar output.
                eprintln!("  {msg}");
            }
            ExecutionEvent::EffectSkipped {
                effect,
                reason,
                progress,
            } => {
                let key = format_effect(effect);
                let counter = format_progress(progress).dimmed();
                let msg = format!(
                    "{} {} - {} {}",
                    "⊘".yellow(),
                    format_effect(effect),
                    reason,
                    counter
                );
                // Skipped effects may not have a spinner (they were never started).
                let mut bars = self.bars.lock().unwrap();
                if let Some(pb) = bars.remove(&key) {
                    pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
                    pb.finish_with_message(msg);
                } else {
                    eprintln!("  {}", msg);
                }
            }
            ExecutionEvent::CascadeUpdateSucceeded { id } => {
                self.multi
                    .println(format!("  {} Update {} (cascade)", "✓".green(), id))
                    .ok();
            }
            ExecutionEvent::CascadeUpdateFailed { id, error } => {
                self.multi
                    .println(format!("  {} Update {} (cascade)", "✗".red(), id))
                    .ok();
                self.multi
                    .println(format!("      {} {}", "→".red(), error.red()))
                    .ok();
            }
            ExecutionEvent::RenameSucceeded { id, from, to } => {
                self.multi
                    .println(format!(
                        "  {} Rename {} \"{}\" → \"{}\"",
                        "✓".green(),
                        id,
                        from,
                        to
                    ))
                    .ok();
            }
            ExecutionEvent::RenameFailed { id, error } => {
                self.multi
                    .println(format!("  {} Rename {}", "✗".red(), id))
                    .ok();
                self.multi
                    .println(format!("      {} {}", "→".red(), error.red()))
                    .ok();
            }
            ExecutionEvent::RefreshStarted => {
                self.multi.println("").ok();
                self.multi
                    .println(format!(
                        "{}",
                        "Refreshing uncertain resource states...".cyan()
                    ))
                    .ok();
            }
            ExecutionEvent::RefreshSucceeded { id } => {
                self.multi
                    .println(format!("  {} Refresh {}", "✓".green(), id))
                    .ok();
            }
            ExecutionEvent::RefreshFailed { id, error } => {
                self.multi
                    .println(format!("  {} Refresh {} - {}", "!".yellow(), id, error))
                    .ok();
            }
        }
    }
}

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
                resource.attributes.insert(
                    attr.clone(),
                    carina_core::resource::Expr(Value::String(value.clone())),
                );
            }
        }
    }
}

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

/// Queue a state refresh for a resource after a failed operation.
///
/// This is kept for use by tests in `tests.rs`. The core executor has its own
/// internal version.
#[cfg(test)]
pub fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
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
    refreshes.sort_by(|(left_id, _), (right_id, _)| left_id.to_string().cmp(&right_id.to_string()));
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

/// Execute import effects by reading the resource from the provider.
///
/// For each Import effect, calls provider.read() with the given identifier
/// to fetch the current state and stores the result in applied_states
/// so that finalize_apply can persist it.
async fn execute_import_effects(plan: &Plan, provider: &dyn Provider, result: &mut ApplyResult) {
    for effect in plan.effects() {
        if let Effect::Import { id, identifier } = effect {
            println!("  {} Importing {} (id: {})...", "<-".cyan(), id, identifier);
            match provider.read(id, Some(identifier)).await {
                Ok(state) => {
                    if state.exists {
                        println!("  {} Imported {}", "✓".green(), id);
                        result.applied_states.insert(id.clone(), state);
                        result.success_count += 1;
                    } else {
                        println!(
                            "  {} Import failed: resource {} with id {} not found",
                            "✗".red(),
                            id,
                            identifier
                        );
                        result.failure_count += 1;
                    }
                }
                Err(e) => {
                    println!("  {} Import failed for {}: {}", "✗".red(), id, e);
                    result.failure_count += 1;
                }
            }
        }
    }
}

/// Execute state-only effects (remove, move) with user feedback.
///
/// These effects only modify state and don't call the provider.
fn execute_state_only_effects(plan: &Plan, result: &mut ApplyResult) {
    for effect in plan.effects() {
        match effect {
            Effect::Remove { id } => {
                println!("  {} Removing {} from state", "x".red(), id);
                result.success_count += 1;
            }
            Effect::Move { from, to } => {
                println!("  {} Moving {} -> {}", "->".yellow(), from, to);
                result.success_count += 1;
            }
            _ => {}
        }
    }
}

/// Input parameters for `finalize_apply`.
///
/// Groups the execution result, resource data, and backend configuration
/// needed to save state after an apply operation.
pub struct FinalizeApplyInput<'a> {
    pub result: &'a ApplyResult,
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub plan: &'a Plan,
    pub backend: &'a dyn StateBackend,
    pub lock: Option<&'a LockInfo>,
    pub schemas: &'a HashMap<String, ResourceSchema>,
}

/// Save state after apply. Does NOT release the lock -- caller is responsible.
///
/// When `lock` is `None` (i.e. `--lock=false`), state is written without lock
/// validation via `save_state_unlocked`.
pub async fn finalize_apply(input: FinalizeApplyInput<'_>) -> Result<(), AppError> {
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

pub struct ApplyStateSave<'a> {
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub applied_states: &'a HashMap<ResourceId, State>,
    pub permanent_name_overrides: &'a HashMap<ResourceId, HashMap<String, String>>,
    pub plan: &'a Plan,
    pub successfully_deleted: &'a HashSet<ResourceId>,
    pub failed_refreshes: &'a HashSet<ResourceId>,
    pub schemas: &'a HashMap<String, ResourceSchema>,
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
        schemas,
    } = save;
    let mut state = state_file.unwrap_or_default();

    for resource in sorted_resources {
        let existing = state.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            &resource.id.name,
        );
        // Collect write-only attribute names from the schema for this resource type.
        // Schema keys include the provider prefix (e.g., "awscc.ec2.vpc"), so we must
        // construct the key the same way as schema_key_for_resource().
        let schema_key = if resource.id.provider.is_empty() {
            resource.id.resource_type.clone()
        } else {
            format!("{}.{}", resource.id.provider, resource.id.resource_type)
        };
        let write_only_keys: Vec<String> = schemas
            .get(&schema_key)
            .map(|schema| {
                schema
                    .attributes
                    .iter()
                    .filter(|(_, attr)| attr.write_only)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();

        if let Some(applied_state) = applied_states.get(&resource.id) {
            let mut resource_state =
                ResourceState::from_provider_state(resource, applied_state, existing)?;
            if let Some(overrides) = permanent_name_overrides.get(&resource.id) {
                resource_state.name_overrides = overrides.clone();
            }
            if !write_only_keys.is_empty() {
                resource_state.merge_write_only_attributes(resource, &write_only_keys);
            }
            state.upsert_resource(resource_state);
        } else if failed_refreshes.contains(&resource.id) {
            continue;
        } else if let Some(current_state) = current_states.get(&resource.id) {
            if current_state.exists {
                let mut resource_state =
                    ResourceState::from_provider_state(resource, current_state, existing)?;
                if !write_only_keys.is_empty() {
                    resource_state.merge_write_only_attributes(resource, &write_only_keys);
                }
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
        match effect {
            Effect::Delete { id, .. } if successfully_deleted.contains(id) => {
                state.remove_resource(&id.provider, &id.resource_type, &id.name);
            }
            Effect::Import { .. } => {
                // Already handled in the sorted_resources loop above via applied_states.
                // Re-upserting here would overwrite metadata (lifecycle, prefixes,
                // desired_keys, binding, dependency_bindings) with bare defaults.
            }
            Effect::Remove { id } => {
                state.remove_resource(&id.provider, &id.resource_type, &id.name);
            }
            Effect::Move { from, to } => {
                // Move: update the resource's identity in state
                if let Some(existing) = state
                    .find_resource(&from.provider, &from.resource_type, &from.name)
                    .cloned()
                {
                    state.remove_resource(&from.provider, &from.resource_type, &from.name);
                    let mut moved_resource = existing;
                    moved_resource.provider = to.provider.clone();
                    moved_resource.resource_type = to.resource_type.clone();
                    moved_resource.name = to.name.clone();
                    state.upsert_resource(moved_resource);
                }
            }
            _ => {}
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

    // All code after lock acquisition is wrapped so that lock release is guaranteed.
    // Ctrl+C cancels the operation and returns Interrupted so the lock is still released.
    let op_result = crate::signal::run_with_ctrl_c(run_apply_locked(
        &ctx,
        &mut parsed,
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

async fn run_apply_locked(
    ctx: &WiringContext,
    parsed: &mut carina_core::parser::ParsedFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    base_dir: &std::path::Path,
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
    let provider = get_provider_with_ctx(ctx, parsed, base_dir).await;

    // Remote state bindings are loaded up front so data source refs that
    // reference `remote_state` blocks can be resolved during refresh (#1683).
    let remote_bindings = super::plan::load_remote_states(&parsed.remote_states, base_dir).await?;

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
    let saved_dep_bindings: HashMap<ResourceId, Vec<String>> = state_file
        .as_ref()
        .map(|sf| {
            sorted_resources
                .iter()
                .filter_map(|r| {
                    let rs = sf.find_resource(&r.id.provider, &r.id.resource_type, &r.id.name)?;
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
    let mut orphan_dependencies: HashMap<ResourceId, Vec<String>> = HashMap::new();
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
            let mut attrs: HashMap<String, Value> = Expr::resolve_map(&resource.attributes);
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
    provider.normalize_desired(&mut resources_for_plan);

    // Normalize state enum values to match the DSL format produced by normalize_desired.
    // Must match the plan path in wiring.rs to ensure plan/apply produce the same diffs.
    provider.normalize_state(&mut current_states);

    // Merge default_tags from provider configs into resources that support tags.
    // Done after normalize_desired so enum values in tags are already resolved.
    // Must match the plan path in wiring.rs to ensure plan/apply produce the same diffs.
    for provider_config in &parsed.providers {
        if !provider_config.default_tags.is_empty() {
            provider.merge_default_tags(
                &mut resources_for_plan,
                &provider_config.default_tags,
                ctx.schemas(),
            );
        }
    }

    // Resolve enum aliases (e.g., "all" -> "-1") in both desired resources
    // and current states so the plan shows canonical AWS values.
    crate::wiring::resolve_enum_aliases_with_ctx(ctx, &mut resources_for_plan);
    crate::wiring::resolve_enum_aliases_in_states(ctx, &mut current_states);

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

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();

    print_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(ctx.schemas()),
        &moved_origins,
    );

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
    );

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
        if let Some(ref binding_name) = resource.binding {
            let mut attrs: HashMap<String, Value> = Expr::resolve_map(&resource.attributes);
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

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::schema::{AttributeSchema, AttributeType};

    #[test]
    fn build_state_after_apply_finds_write_only_with_provider_prefix() {
        // The schema map is keyed by provider-prefixed names (e.g., "awscc.ec2.vpc"),
        // but the buggy code used resource.id.resource_type (e.g., "ec2.vpc") for lookup.
        // This test verifies that write-only attributes are found when the schema key
        // includes the provider prefix.
        let mut schemas = HashMap::new();
        let schema = ResourceSchema::new("ec2.vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
            .attribute(
                AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only(),
            );
        // Schema is registered with provider-prefixed key
        schemas.insert("awscc.ec2.vpc".to_string(), schema);

        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "my-vpc");
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
            .find_resource("awscc", "ec2.vpc", "my-vpc")
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
}
