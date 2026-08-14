use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use colored::Colorize;
use futures::stream::{FuturesUnordered, StreamExt};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::effect::Effect;
use carina_core::executor::UnresolvedDataSourceInput;
use carina_core::parser::ProviderContext;
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer, RawSavedAttrs};
use carina_core::resource::{
    ConcreteValue, DataSource, ResolvedDataSource, Resource, ResourceId, State, Value,
};
use carina_core::shutdown::{ShutdownPhase, ShutdownToken};
use carina_core::value::{format_value, json_to_dsl_value};
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, DeposedInstance, DeposedKey, LockInfo,
    ResourceState, StateBackend, StateFile, StateUrl, create_backend, create_remote_backend,
    load_state_from_url, resolve_backend_for_read,
};

use super::{BackendDriftStatus, DriftCommand, inspect_backend_drift, verify_for_mutation};
use crate::commands::shared::finalize::release_lock_after_execute;
use crate::commands::shared::state_writeback::{SkippedExports, apply_name_overrides};
use crate::error::AppError;
use crate::wiring::{
    DataSourceRefreshResolution, WiringContext, build_factories_from_providers,
    get_provider_with_ctx, read_data_source_with_retry, reconcile_anonymous_identifiers_with_ctx,
    reconcile_prefixed_names, resolve_data_source_refs_for_refresh,
};

fn shutdown_requested(shutdown: &ShutdownToken) -> bool {
    match shutdown.phase() {
        ShutdownPhase::Running => false,
        ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => true,
    }
}

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

fn format_deferred_state_refresh_warning(
    resource: &DataSource,
    unresolved: &[UnresolvedDataSourceInput],
) -> String {
    let mut refs = unresolved
        .iter()
        .flat_map(|input| {
            let paths = input.paths.iter().map(|path| path.to_dot_string());
            let bindings = input.bindings.iter().cloned();
            let unknowns = input
                .unknowns
                .iter()
                .map(|reason| format!("unknown({reason})"));
            paths.chain(bindings).chain(unknowns)
        })
        .collect::<Vec<_>>();
    if refs.is_empty() {
        refs.extend(resource.dependency_bindings.iter().cloned());
    }
    let refs = if refs.is_empty() {
        "unknown input dependency".to_string()
    } else {
        refs.join(", ")
    };
    format!(
        "Warning: skipped refreshing data source {} because its inputs depend on \
         missing or deferred state bindings: {refs}",
        resource.id
    )
}

/// Read local state file for shell completion.
///
/// Tries `carina.state.json` in the current directory. Missing or invalid files
/// degrade silently to no candidates. Older files are migrated in memory;
/// future versions print the version-gate error and produce no candidates.
fn read_local_state_for_completion() -> Option<StateFile> {
    let path = std::path::Path::new("carina.state.json");
    let contents = std::fs::read_to_string(path).ok()?;
    match parse_local_state_for_completion(&contents) {
        Ok(state) => Some(state),
        Err(e @ BackendError::StateVersionTooNew { .. }) => {
            eprintln!("{e}");
            None
        }
        Err(_) => None,
    }
}

fn parse_local_state_for_completion(contents: &str) -> Result<StateFile, BackendError> {
    carina_state::check_and_migrate(contents).map(|migrated| migrated.into_state())
}

/// Shell completion function for `state lookup` queries.
///
/// Delegates to [`complete_state_lookup_from`], which produces three
/// candidate spaces: resource bindings/names (module-prefixed
/// bindings like `r.distribution` included), attribute names for a
/// resolved binding (longest-prefix match), and the `exports` /
/// `exports.<key>` address shapes when the state carries exports.
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
///
/// Three candidate spaces are produced (carina#3338):
///
/// - **Resource bindings / identities**: surface every binding the state
///   carries — module-prefixed (`r.distribution`) shows up as one
///   candidate, just like `state list` already prints it. No splitting
///   on `.`; matched by `starts_with(current)`. (Top-level identities
///   with no binding fall back to `rs.identity`.)
/// - **`exports.<key>`** when the partial starts with `exports.` and
///   no resource binding `exports` shadows it.
/// - **`exports`** as a top-level candidate when the state has any
///   exports and the partial matches it.
fn complete_state_lookup_from(state: &StateFile, current: &str) -> Vec<CompletionCandidate> {
    let resource_named_exports = state
        .resources()
        .iter()
        .any(|r| r.binding.as_deref() == Some("exports"));

    // `exports.<key>` per-export completion. Only when no resource has
    // claimed the `exports` binding — that resource takes precedence
    // (matches `format_state_lookup`).
    if !resource_named_exports && let Some(prefix) = current.strip_prefix("exports.") {
        return state
            .exports
            .keys()
            .filter(|key| key.starts_with(prefix))
            .map(|key| CompletionCandidate::new(format!("exports.{}", key)))
            .collect();
    }

    // Attribute completion for a known resource: `<binding>.<attr>`.
    // Use the address resolver so module-prefixed bindings ride the
    // same longest-prefix rule as `format_state_lookup`.
    if let Some((before_dot, _)) = current.rsplit_once('.')
        && let Some((rs, _)) = resolve_resource_address(state, before_dot)
    {
        return rs
            .attributes
            .keys()
            .filter(|key| {
                let full = format!("{}.{}", before_dot, key);
                full.starts_with(current)
            })
            .map(|key| CompletionCandidate::new(format!("{}.{}", before_dot, key)))
            .collect();
    }

    // Top-level: resource bindings/identities + optional `exports`.
    let mut candidates: Vec<CompletionCandidate> = Vec::new();
    for rs in state.resources() {
        let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
        if display_name.starts_with(current) {
            candidates.push(CompletionCandidate::new(display_name));
        }
    }
    if !resource_named_exports && !state.exports.is_empty() && "exports".starts_with(current) {
        candidates.push(CompletionCandidate::new("exports"));
    }
    candidates
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
        /// Path to directory containing .crn files (defaults to ".").
        /// Mutually exclusive with --state-url.
        path: Option<PathBuf>,

        /// Read state directly from a URL, bypassing .crn / backend
        /// resolution. Accepts s3://bucket/key, file://path, or a bare
        /// local path. Mutually exclusive with [PATH].
        #[arg(long, conflicts_with = "path")]
        state_url: Option<String>,
    },
    /// Look up resource attributes from the state file
    Lookup {
        /// Query: <binding_or_name> for full resource, <binding_or_name>.<attribute> for specific attribute
        #[arg(add = ArgValueCompleter::new(complete_state_lookup))]
        query: String,

        /// Path to directory containing .crn files (defaults to ".").
        /// Mutually exclusive with --state-url.
        path: Option<PathBuf>,

        /// Read state directly from a URL, bypassing .crn / backend
        /// resolution. Accepts s3://bucket/key, file://path, or a bare
        /// local path. Mutually exclusive with [PATH].
        #[arg(long, conflicts_with = "path")]
        state_url: Option<String>,

        /// Always output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show all managed resources with full attributes
    Show {
        /// Path to directory containing .crn files (defaults to ".").
        /// Mutually exclusive with --state-url.
        path: Option<PathBuf>,

        /// Read state directly from a URL, bypassing .crn / backend
        /// resolution. Accepts s3://bucket/key, file://path, or a bare
        /// local path. Mutually exclusive with [PATH].
        #[arg(long, conflicts_with = "path")]
        state_url: Option<String>,

        /// Display state in interactive TUI mode
        #[arg(long)]
        tui: bool,

        /// Output state as JSON
        #[arg(long)]
        json: bool,
    },
}

/// Run state subcommands
pub async fn run_state_command(
    command: StateCommands,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    match command {
        StateCommands::BucketDelete {
            bucket_name,
            force,
            path,
        } => run_state_bucket_delete(&bucket_name, force, &path, provider_context).await,
        StateCommands::Refresh { path, lock } => {
            run_state_refresh(&path, lock, provider_context, cancel).await
        }
        StateCommands::List { path, state_url } => {
            run_state_list(path.as_deref(), state_url.as_deref(), provider_context).await
        }
        StateCommands::Lookup {
            query,
            path,
            state_url,
            json,
        } => {
            run_state_lookup(
                &query,
                path.as_deref(),
                state_url.as_deref(),
                json,
                provider_context,
            )
            .await
        }
        StateCommands::Show {
            path,
            state_url,
            tui,
            json,
        } => {
            run_state_show(
                path.as_deref(),
                state_url.as_deref(),
                tui,
                json,
                provider_context,
            )
            .await
        }
    }
}

/// Run force-unlock command
pub async fn run_force_unlock(
    lock_id: &str,
    path: &Path,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let parsed = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?
    .parsed;
    let base_dir = get_base_dir(path);

    let backend_config = match inspect_backend_drift(base_dir, parsed.backend.as_ref())? {
        BackendDriftStatus::Drifted { existing, .. } => Some(existing.to_state_config()),
        BackendDriftStatus::Fresh | BackendDriftStatus::Unchanged => {
            parsed.backend.as_ref().map(StateBackendConfig::from)
        }
    };

    // Bypasses verify_for_mutation by design. On drift, targets the OLD locked backend so users can unlock a stale migration.
    let backend: Box<dyn StateBackend> = create_backend(backend_config.as_ref(), base_dir)
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

/// Load the state file, either from a configured backend (resolved via
/// the .crn at `path`) or from a direct URL.
///
/// `path` and `state_url` are mutually exclusive at the clap layer
/// (`conflicts_with = "path"`), so this helper trusts that at most one
/// is `Some`. When both are `None`, the existing behavior applies and
/// the path defaults to `.`.
async fn load_state_file(
    path: Option<&Path>,
    state_url: Option<&str>,
    provider_context: &ProviderContext,
) -> Result<StateFile, AppError> {
    if let Some(raw) = state_url {
        let url = StateUrl::parse(raw).map_err(AppError::Backend)?;
        return load_state_from_url(&url).await.map_err(AppError::Backend);
    }

    let default_path = PathBuf::from(".");
    let path = path.unwrap_or(&default_path);

    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    let backend: Box<dyn StateBackend> =
        resolve_backend_for_read(parsed.backend.as_ref(), base_dir)
            .await
            .map_err(AppError::Backend)?;

    let state_file = backend.read_state().await.map_err(AppError::Backend)?;
    state_file
        .map(|loaded| loaded.into_state())
        .ok_or_else(|| AppError::Config("No state file found.".to_string()))
}

/// Find a resource by binding name first, then fall back to resource
/// identity. Retained for test coverage of the precedence rule; production
/// lookup uses [`resolve_resource_address`] which generalizes this with
/// longest-prefix matching for module-prefixed bindings (carina#3338).
#[cfg(test)]
fn find_resource_by_query<'a>(state: &'a StateFile, name: &str) -> Option<&'a ResourceState> {
    // Search by binding first
    state
        .resources()
        .iter()
        .find(|r| r.binding.as_deref() == Some(name))
        .or_else(|| {
            // Fall back to identity
            state.resources().iter().find(|r| r.identity == name)
        })
}

/// Format state list output. Returns each line as a string.
fn format_state_list(state: &StateFile) -> Vec<String> {
    let mut lines = Vec::new();
    for rs in state.resources() {
        let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
        let row_prefix = format!("{}.{} {}", rs.provider, rs.resource_type, display_name);
        if rs.identifier.is_none() {
            lines.push(format!("{row_prefix}  (no current instance)"));
        } else {
            lines.push(row_prefix.clone());
        }
        for deposed in &rs.deposed {
            lines.push(format!(
                "{}  {}",
                row_prefix,
                deposed_state_marker(&deposed.key, &deposed.identifier)
            ));
        }
    }
    lines
}

fn deposed_state_marker(key: &DeposedKey, identifier: &str) -> String {
    format!("(deposed {key} {identifier})")
}

/// Run state list command
async fn run_state_list(
    path: Option<&Path>,
    state_url: Option<&str>,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, state_url, provider_context).await?;

    if state.resources().is_empty() {
        println!("No resources in state.");
        return Ok(());
    }

    for line in format_state_list(&state) {
        println!("{}", line);
    }

    Ok(())
}

/// Format lookup output for a query against a state file.
///
/// Three address shapes are accepted (in resolution order):
///
/// 1. **Resource binding / name**, optionally followed by an attribute:
///    `vpc`, `vpc.vpc_id`, `r.distribution`, `r.distribution.id`. The
///    binding is matched by **longest-prefix**, so module-prefixed
///    bindings (`let r = usecase { … }` → resources stored as
///    `binding = "r.distribution"`) resolve the same way `state list`
///    already displays them (carina#3338). The longest-prefix scan
///    also subsumes the previous one-level form.
/// 2. **`exports`** (full state.exports map) or **`exports.<key>`**
///    (single export value). The deliberate downstream contract for
///    CI / scripting consumers — a resource named `exports` still
///    takes precedence (rule 1 runs first), so the export form only
///    kicks in when no such binding exists.
/// 3. When neither (1) nor (2) matches, the error names the full
///    query as the operator typed it — so for a mistyped
///    `r.distribution.idd` the message is "Resource 'r.distribution.idd'
///    not found", not a stripped head.
fn format_state_lookup(
    state: &StateFile,
    query: &str,
    json_output: bool,
) -> Result<String, AppError> {
    // (1) Longest-binding-prefix match against resources.
    if let Some((rs, attribute)) = resolve_resource_address(state, query) {
        return format_resource_value(rs, attribute, json_output);
    }

    // (2) Exports — only when no resource named `exports` shadowed
    // step (1) above (the loop would have matched it). The whole-map
    // form is `exports`; per-key is `exports.<key>`.
    if query == "exports" {
        return Ok(serde_json::to_string_pretty(&sorted_exports(state)).unwrap());
    }
    if let Some(key) = query.strip_prefix("exports.") {
        let value = state
            .exports
            .get(key)
            .ok_or_else(|| AppError::Config(format!("Export key '{}' not found in state.", key)))?;
        return if json_output {
            Ok(serde_json::to_string_pretty(value).unwrap())
        } else {
            Ok(format_raw_value(value))
        };
    }

    // (3) Nothing matched — report the full query so the operator
    // sees the address they typed, not a stripped head.
    Err(AppError::Config(format!(
        "Resource '{}' not found in state.",
        query
    )))
}

/// Build a sorted view of `state.exports` for deterministic JSON output.
fn sorted_exports(state: &StateFile) -> std::collections::BTreeMap<&String, &serde_json::Value> {
    state.exports.iter().collect()
}

/// Resolve a query of the form `<binding>` or `<binding>.<attribute>`
/// against the state's resources, picking the **longest** binding that
/// matches a `<binding>` or `<binding>.<rest>` prefix of the query.
///
/// Returns `(resource, optional_attribute_name)`. The longest-prefix
/// rule lets module-prefixed bindings (`binding = "r.distribution"`)
/// resolve `r.distribution.id` → ("r.distribution", "id"), while a
/// top-level `binding = "vpc"` still resolves `vpc.vpc_id` →
/// ("vpc", "vpc_id"). Returns `None` if no binding matches a prefix.
fn resolve_resource_address<'a>(
    state: &'a StateFile,
    query: &'a str,
) -> Option<(&'a ResourceState, Option<&'a str>)> {
    // Walk all resources, keep the one whose binding (or fallback
    // name) matches the longest prefix of `query`. Equal-length
    // candidates: binding wins over name (matches the historical
    // `find_resource_by_query` precedence).
    let mut best: Option<(&'a ResourceState, &'a str, bool)> = None;
    for rs in state.resources() {
        for (candidate, is_binding) in candidate_addresses(rs) {
            if query_starts_with_address(query, candidate) {
                let take = match &best {
                    None => true,
                    Some((_, prev, prev_is_binding)) => {
                        candidate.len() > prev.len()
                            || (candidate.len() == prev.len() && is_binding && !*prev_is_binding)
                    }
                };
                if take {
                    best = Some((rs, candidate, is_binding));
                }
            }
        }
    }

    let (rs, matched, _) = best?;
    let attribute = if query.len() == matched.len() {
        None
    } else {
        // matched is a strict prefix; the byte after it must be '.'
        // (guaranteed by query_starts_with_address).
        Some(&query[matched.len() + 1..])
    };
    Some((rs, attribute))
}

/// Candidate addresses for a resource, paired with `is_binding` so the
/// longest-prefix tie-break can prefer bindings over identities.
fn candidate_addresses(rs: &ResourceState) -> impl Iterator<Item = (&str, bool)> {
    rs.binding
        .as_deref()
        .map(|b| (b, true))
        .into_iter()
        .chain(std::iter::once((rs.identity.as_str(), false)))
}

/// `true` if `query` is exactly `address` or starts with `address` + '.'.
/// Bare substring `starts_with` would mis-match `r.distribution_v2`
/// against binding `r.distribution`.
fn query_starts_with_address(query: &str, address: &str) -> bool {
    if query == address {
        return true;
    }
    let rest = match query.strip_prefix(address) {
        Some(r) => r,
        None => return false,
    };
    rest.starts_with('.')
}

/// Render a resource attribute (or the full sorted attribute map when
/// `attribute` is `None`) in the same shape `format_state_lookup`
/// historically produced.
fn format_resource_value(
    rs: &ResourceState,
    attribute: Option<&str>,
    json_output: bool,
) -> Result<String, AppError> {
    match attribute {
        Some(attr) => format_resource_attribute_value(rs, attr, json_output),
        None if !rs.deposed.is_empty() => format_resource_full_value_with_deposed(rs),
        None => {
            let sorted: std::collections::BTreeMap<_, _> = rs.attributes.iter().collect();
            Ok(serde_json::to_string_pretty(&sorted).unwrap())
        }
    }
}

fn format_resource_attribute_value(
    rs: &ResourceState,
    attr: &str,
    json_output: bool,
) -> Result<String, AppError> {
    let Some(value) = rs.attributes.get(attr) else {
        return missing_attribute_error(rs, attr);
    };
    if json_output {
        Ok(serde_json::to_string_pretty(value).unwrap())
    } else {
        Ok(format_raw_value(value))
    }
}

fn format_resource_full_value_with_deposed(rs: &ResourceState) -> Result<String, AppError> {
    let current = current_full_value(rs);
    let deposed: Vec<serde_json::Value> = rs.deposed.iter().map(deposed_full_value).collect();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "current": current,
        "deposed": deposed,
    }))
    .unwrap())
}

fn missing_attribute_error<T>(rs: &ResourceState, attr: &str) -> Result<T, AppError> {
    let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
    Err(AppError::Config(format!(
        "Attribute '{}' not found on resource '{}'.",
        attr, display_name
    )))
}

fn sorted_attributes_value(attributes: &HashMap<String, serde_json::Value>) -> serde_json::Value {
    let sorted: std::collections::BTreeMap<_, _> = attributes.iter().collect();
    serde_json::to_value(sorted).unwrap()
}

fn current_full_value(rs: &ResourceState) -> serde_json::Value {
    if rs.identifier.is_none() && rs.attributes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({
            "identifier": rs.identifier.as_ref(),
            "attributes": sorted_attributes_value(&rs.attributes),
        })
    }
}

fn deposed_full_value(entry: &DeposedInstance) -> serde_json::Value {
    serde_json::json!({
        "key": entry.key.as_str(),
        "identifier": entry.identifier,
        "marker": deposed_state_marker(&entry.key, &entry.identifier),
        "provider_instance": entry.provider_instance,
        "attributes": sorted_attributes_value(&entry.attributes),
    })
}

/// Run state lookup command
async fn run_state_lookup(
    query: &str,
    path: Option<&Path>,
    state_url: Option<&str>,
    json_output: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, state_url, provider_context).await?;
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
    for rs in state.resources() {
        // carina#3181 PR D: `Effect::Read` carries a `DataSource`.
        let mut resource = carina_core::resource::DataSource::with_provider(
            &rs.provider,
            &rs.resource_type,
            &rs.identity,
            rs.directives.provider_instance.clone(),
        );
        resource.directives = rs.directives.clone();

        // Set typed metadata fields from state
        resource.binding = rs.binding.clone();
        resource.dependency_bindings = rs.dependency_bindings.clone();

        // Convert JSON attributes to DSL values
        for (key, json_val) in &rs.attributes {
            if let Some(dsl_val) = json_to_dsl_value(json_val) {
                resource.set_attr(key.clone(), dsl_val);
            }
        }

        plan.add(Effect::Read {
            resource: ResolvedDataSource::new(resource),
        });
    }
    plan
}

/// Format state show output (non-TUI mode).
///
/// Shows all resources with their type, identity/binding, and full attributes.
fn format_state_show(state: &StateFile) -> String {
    let mut output = String::new();
    for (i, rs) in state.resources().iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
        output.push_str(&format!(
            "# {}.{} ({})\n",
            rs.provider, rs.resource_type, display_name
        ));

        format_attributes_for_show(&mut output, &rs.attributes, "  ");
        for deposed in &rs.deposed {
            output.push_str(&format!(
                "  {}\n",
                deposed_state_marker(&deposed.key, &deposed.identifier)
            ));
            format_attributes_for_show(&mut output, &deposed.attributes, "    ");
        }
    }
    output
}

fn format_attributes_for_show(
    output: &mut String,
    attributes: &HashMap<String, serde_json::Value>,
    indent: &str,
) {
    let mut keys: Vec<&String> = attributes.keys().collect();
    keys.sort();
    for key in keys {
        let value = &attributes[key];
        if let Some(dsl_val) = json_to_dsl_value(value) {
            output.push_str(&format!("{}{} = {}\n", indent, key, format_value(&dsl_val)));
        }
    }
}

/// Run state show command
async fn run_state_show(
    path: Option<&Path>,
    state_url: Option<&str>,
    tui: bool,
    json: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, state_url, provider_context).await?;

    if json {
        let json_str = serde_json::to_string_pretty(&state)
            .map_err(|e| format!("Failed to serialize state: {}", e))?;
        println!("{}", json_str);
        return Ok(());
    }

    if state.resources().is_empty() {
        println!("No resources in state.");
        return Ok(());
    }

    if tui {
        let plan = build_plan_from_state(&state);
        carina_tui::run(&plan, &carina_core::schema::SchemaRegistry::new())
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
    path: &Path,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let parsed = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?
    .parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found.")?;

    // Verify the bucket name matches the backend configuration
    let config_bucket = backend_config
        .attributes
        .get("bucket")
        .and_then(|v| match v {
            Value::Concrete(ConcreteValue::String(s)) => Some(s.as_str()),
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

    // Bypasses verify_for_mutation by design. The configured-bucket-name guard pins this to the NEW backend; for an orphaned OLD bucket, delete manually via the cloud console.
    // Create backend to get provider metadata
    let state_config = StateBackendConfig::from(backend_config);
    let backend = create_remote_backend(&state_config)
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
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir)?;
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
    let bucket_provider = factory
        .create_provider(None, &provider_config_attrs)
        .await?;

    // First, try to empty the bucket (delete all objects and versions)
    println!();
    println!("{}", "Emptying bucket...".cyan());

    // Delete the bucket resource (identifier is the bucket name)
    // Backend bucket is provider-default; named-instance routing is
    // a DSL concern that doesn't apply to the implicit state bucket.
    let bucket_id = ResourceId::with_provider_identity(
        backend_provider_name,
        backend_resource_type,
        bucket_name,
        None,
    );
    match bucket_provider
        .delete(
            &bucket_id,
            bucket_name,
            carina_core::provider::DeleteRequest::default(),
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
    path: &Path,
    lock: bool,
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

    let verified_backend = verify_for_mutation(
        base_dir,
        parsed.backend.as_ref(),
        DriftCommand::RefreshState,
    )?;

    // Create backend
    let backend: Box<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?;

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

    let op_result = run_state_refresh_locked(
        &mut parsed,
        backend.as_ref(),
        lock_info.as_ref(),
        base_dir,
        cancel.clone(),
    )
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = release_lock_after_execute(backend.as_ref(), li, &cancel).await;

        if release_result.is_ok() && matches!(op_result, Err(AppError::Interrupted)) {
            println!("  {} Lock released", "✓".green());
        }

        op_result?;
        release_result
    } else {
        op_result
    }
}

pub(crate) async fn run_state_refresh_locked(
    parsed: &mut carina_core::parser::InferredFile,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    base_dir: &std::path::Path,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir)?;
    let ctx = WiringContext::new(factories);

    // Read current state from backend. carina#3315: persist any older-schema
    // migration under the refresh lock before the "no
    // resources" short-circuit returns — see
    // `apply::load_state_persist_if_migrated`. The on-disk version
    // must advance so the carina#3283 warning text matches reality.
    let mut state_file =
        crate::commands::apply::load_state_persist_if_migrated(backend, lock).await?;

    if state_file.as_ref().is_none_or(|s| s.resources().is_empty()) {
        let msg = if state_file.is_none() {
            "No state file found. Nothing to refresh."
        } else {
            "No resources in state. Nothing to refresh."
        };
        println!("{}", msg.yellow());
        return Ok(());
    }

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    let state_block_claims = crate::wiring::resolve_state_block_claims(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        ctx.schemas(),
    );
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(
            &ctx,
            &mut parsed.resources,
            sf,
            &state_block_claims,
        )?;
    }
    // state is a read-only inspection command and does not run the differ. The
    // state-side name_overrides are sufficient for this narrow display path; the
    // full resolver -> override -> bindings rebuild -> second-pass resolver
    // sequence is unnecessary here. Sub-PR B leaves this call site as-is for
    // that reason. See
    // notes/specs/2026-06-28-issue-3625-cbd-decompose-clean-design.md Phase 5 T5.8.
    apply_name_overrides(&mut parsed.resources, &state_file);

    let mut sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;

    // Select provider
    let provider = get_provider_with_ctx(&ctx, parsed, base_dir).await?;

    println!();
    println!("{}", "Refreshing state...".cyan().bold());

    // Read states for all resources using identifier from state.
    // Cancel stops dispatching new reads, then waits for in-flight reads to finish
    // so provider futures are not dropped mid-call.
    let managed_reads: Vec<(ResourceId, String)> = sorted_resources
        .iter()
        .filter_map(|resource| {
            let identifier = state_file
                .as_ref()
                .and_then(|sf| sf.get_identifier_for_resource(resource))?;
            Some((resource.id.clone(), identifier))
        })
        .collect();
    let (mut current_states, already_refreshed) =
        refresh_existing_resources_until_cancelled(&provider, managed_reads, &cancel).await?;
    if shutdown_requested(&cancel) {
        return Err(AppError::Interrupted);
    }

    // carina#3272: expand `for _, _ in <iter> { ... }` loops the same
    // way `run_apply_locked` does, so the materialised children land
    // in `sorted_resources` (and therefore in the orphan-classification
    // `desired_ids` set below + the `lift_current_state_enum_leaves`
    // input slice). Without this, every for-loop-produced resource is
    // mis-classified as `(orphan)` on refresh and its enum-typed attrs
    // skip the Enum lift, surfacing snake_case ↔ SCREAMING_CASE
    // as a phantom `~` diff.
    //
    // Refresh has no `moved` block (that is a plan/apply concept), so
    // `moved_targets` is empty. `already_refreshed` carries the ids the
    // managed read loop above already populated, so the post-expansion
    // refresh below doesn't redundantly re-read them.
    let wait_aliases_for_expansion: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    // carina#3278: route the expand → child-refresh → hydrate(2nd) →
    // lift quartet through the shared constructor so this path and
    // `run_apply_locked` cannot drift on the sequence again.
    // carina#3739: `build_saved_attrs` returns raw persisted values; consuming
    // them through `lift` is required before the typed expansion input accepts
    // the map for Phase 2.5 hydration.
    let saved_attrs_for_expansion = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default()
        .lift(ctx.schemas());
    let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
        .as_ref()
        .map(|sf| {
            sorted_resources
                .iter()
                .filter_map(|r| {
                    let rs = sf.find_resource(
                        &r.id.provider,
                        &r.id.resource_type,
                        r.id.identity_or_empty(),
                    )?;
                    if rs.dependency_bindings.is_empty() {
                        None
                    } else {
                        Some((r.id.clone(), rs.dependency_bindings.clone()))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let multi = indicatif::MultiProgress::new();
    let crate::wiring::ExpandedRefreshState {
        sorted_resources: resorted,
        new_child_ids: _,
        refreshable_child_ids: _,
        residual_deferred_for: _,
        deferred_create_targets: _,
        printed_warnings: _,
    } = crate::wiring::expand_refresh_and_lift_states(crate::wiring::ExpandRefreshAndLiftInputs {
        parsed,
        provider: &provider,
        sorted_resources: &sorted_resources,
        current_states: &mut current_states,
        remote_bindings: &HashMap::new(),
        wait_aliases: &wait_aliases_for_expansion,
        moved_targets: &HashSet::new(),
        already_refreshed: &already_refreshed,
        state_file: &state_file,
        saved_dep_bindings: &saved_dep_bindings,
        saved_attrs: &saved_attrs_for_expansion,
        multi: &multi,
        schemas: ctx.schemas(),
    })
    .await?;
    sorted_resources = resorted;
    if shutdown_requested(&cancel) {
        return Err(AppError::Interrupted);
    }

    // Also read states for orphaned resources (in state but removed from config)
    let desired_ids: HashSet<ResourceId> = sorted_resources.iter().map(|r| r.id.clone()).collect();
    let orphan_ids: Vec<(ResourceId, String)> = state_file
        .as_ref()
        .map(|sf| {
            sf.resources()
                .iter()
                .filter_map(|rs| {
                    let id = ResourceId::with_provider_name_compat(
                        &rs.provider,
                        &rs.resource_type,
                        &rs.identity,
                        rs.directives.provider_instance.clone(),
                    );
                    if desired_ids.contains(&id) {
                        return None;
                    }
                    rs.identifier.as_ref().map(|ident| (id, ident.clone()))
                })
                .collect()
        })
        .unwrap_or_default();

    let orphan_states =
        refresh_existing_resources_until_cancelled(&provider, orphan_ids.clone(), &cancel)
            .await?
            .0;
    for (id, fresh_state) in orphan_states {
        current_states.insert(id, fresh_state);
    }

    // carina#3271: re-read every `read aws.*` data source. Without
    // this, `current_states` has no entry for any data source and
    // the downstream `resolve_exports` (after #3266) cannot resolve
    // `<data_source>.<attr>` references in `exports {}`, so
    // `state.exports` keeps the pre-refresh literal for any export
    // whose value depends on a data source.
    //
    // Mirrors the data-source phase of `run_apply_locked`
    // (`resolve_data_source_refs_for_refresh` + `read_data_source_with_retry`):
    // resolve input attribute `ResourceRef`s against the
    // already-refreshed managed `current_states`, then read each
    // data source through the provider.
    if !parsed.data_sources.is_empty() {
        let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
            .wait_bindings
            .iter()
            .map(carina_core::binding_index::WaitAliasSpec::from)
            .collect();
        let data_source_refreshes = resolve_data_source_refs_for_refresh(
            &sorted_resources,
            &parsed.compositions,
            &parsed.data_sources,
            &current_states,
            &HashMap::new(),
            ctx.schemas(),
            &wait_aliases,
        )?;
        for resolution in data_source_refreshes {
            let resource = match resolution {
                DataSourceRefreshResolution::Resolved(resource) => resource,
                DataSourceRefreshResolution::DeferredToApply {
                    resource,
                    unresolved,
                } => {
                    eprintln!(
                        "{}",
                        format_deferred_state_refresh_warning(&resource, &unresolved).yellow()
                    );
                    continue;
                }
            };
            if shutdown_requested(&cancel) {
                return Err(AppError::Interrupted);
            }
            let fresh_state = read_data_source_with_retry(&provider, &resource)
                .await
                .map_err(AppError::Provider)?;
            if shutdown_requested(&cancel) {
                return Err(AppError::Interrupted);
            }
            current_states.insert(resource.id.clone(), fresh_state);
        }
    }

    // Restore unreturned attributes from the same lifted map used by the
    // expansion hydration above (CloudControl doesn't always return them).
    let saved_attrs = saved_attrs_for_expansion;
    provider
        .hydrate_read_state(&mut current_states, saved_attrs.as_provider_saved_attrs())
        .await;
    // awscc#251: also lift the provider-read `current_states` (not just
    // `saved_attrs`) — the values read at the refresh loop above arrive
    // as plain `String` for IAM enum fields and must be lifted before
    // they are written back / compared.
    // carina#3272: same `sorted_resources` reason as above.
    carina_core::utils::lift_current_state_enum_leaves(
        &mut current_states,
        &sorted_resources,
        ctx.schemas(),
    );

    let mut state = state_file.take().unwrap();

    println!();

    let mut refresh_counts = StateRefreshCounts::default();

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
            ctx.schemas(),
            "",
            &mut refresh_counts,
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
            ctx.schemas(),
            " (orphan)",
            &mut refresh_counts,
        )?;
    }

    let deposed_summary = refresh_deposed_generations_until_cancelled(
        &provider,
        &mut state,
        &sorted_resources,
        ctx.schemas(),
        &cancel,
    )
    .await?;
    if shutdown_requested(&cancel) {
        return Err(AppError::Interrupted);
    }

    // Re-resolve exports using refreshed state
    let skipped_exports = if !parsed.export_params.is_empty() {
        let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
            .wait_bindings
            .iter()
            .map(carina_core::binding_index::WaitAliasSpec::from)
            .collect();
        // State refresh path: no head-of-pipeline resolver pass has run,
        // so `parsed.compositions` still carry the authored
        // `ResourceRef` snapshots that `resolve_exports`'s post-apply
        // re-resolution needs (#3169 / #3177).
        let post_apply_states =
            crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
                &current_states,
                &state,
            );
        let resolution = crate::commands::shared::state_writeback::resolve_exports(
            &parsed.export_params,
            &sorted_resources,
            &parsed.data_sources,
            &parsed.compositions,
            &post_apply_states,
            ctx.schemas(),
            &wait_aliases,
        )?;
        resolution.write_into(&mut state)
    } else {
        SkippedExports::default()
    };

    // Save state (with or without lock validation)
    if let Some(lock) = lock {
        crate::commands::apply::save_state_locked(backend, lock, &mut state).await?;
    } else {
        crate::commands::apply::save_state_unlocked(backend, &mut state).await?;
    }

    // Summary
    println!(
        "{}",
        format_state_refresh_summary(
            refresh_counts.updated,
            refresh_counts.unchanged,
            &deposed_summary,
        )
    );
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    if !skipped_exports.is_empty() {
        println!("  {} {}", "!".yellow(), skipped_exports.count_description());
        return Err(AppError::Config(skipped_exports.failure_message()));
    }

    Ok(())
}

fn format_state_refresh_summary(
    updated_count: u32,
    unchanged_count: u32,
    deposed_summary: &DeposedRefreshSummary,
) -> String {
    let base = format!(
        "State refreshed: {} resource{} updated, {} resource{} unchanged",
        updated_count,
        if updated_count == 1 { "" } else { "s" },
        unchanged_count,
        if unchanged_count == 1 { "" } else { "s" },
    );
    if deposed_summary.total_generations == 0 {
        return format!("{base}.");
    }
    let reconciled = deposed_summary.removed_generations + deposed_summary.updated_generations;
    let unchanged_generations = deposed_summary
        .total_generations
        .saturating_sub(reconciled + deposed_summary.failed_generations);
    let failure_suffix = if deposed_summary.failed_generations == 0 {
        String::new()
    } else {
        format!(", {} failed", deposed_summary.failed_generations,)
    };
    format!(
        "{base}, {reconciled} deposed generation{} reconciled ({} removed, {} updated, {} unchanged){failure_suffix}.",
        if reconciled == 1 { "" } else { "s" },
        deposed_summary.removed_generations,
        deposed_summary.updated_generations,
        unchanged_generations,
    )
}

async fn refresh_existing_resources_until_cancelled(
    provider: &dyn Provider,
    reads: Vec<(ResourceId, String)>,
    cancel: &ShutdownToken,
) -> Result<(HashMap<ResourceId, State>, HashSet<ResourceId>), AppError> {
    let mut current_states = HashMap::new();
    let mut refreshed = HashSet::new();
    let mut read_iter = reads.into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut refresh_cancelled = shutdown_requested(cancel);

    loop {
        while !refresh_cancelled && in_flight.len() < 5 {
            let Some((id, identifier)) = read_iter.next() else {
                break;
            };
            in_flight.push(async move {
                let fresh_state = provider
                    .read(
                        &id,
                        Some(identifier.as_str()),
                        carina_core::provider::ReadRequest,
                    )
                    .await
                    .map_err(AppError::Provider)?;
                Ok((id, fresh_state))
            });
        }

        if in_flight.is_empty() {
            break;
        }

        let result: Result<(ResourceId, State), AppError> = if refresh_cancelled {
            tokio::select! {
                biased;
                _ = cancel.cleanup_priority_requested() => break,
                result = in_flight.next() => result.unwrap(),
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cleanup_priority_requested() => {
                    refresh_cancelled = true;
                    break;
                }
                _ = cancel.cancelled() => {
                    refresh_cancelled = true;
                    continue;
                }
                result = in_flight.next() => {
                    result.unwrap()
                }
            }
        };

        if refresh_cancelled {
            continue;
        }

        let (id, state) = result?;
        refreshed.insert(id.clone());
        current_states.insert(id, state);
    }

    drop(in_flight);
    drop(read_iter);

    if refresh_cancelled {
        return Err(AppError::Interrupted);
    }

    Ok((current_states, refreshed))
}

#[derive(Clone)]
struct DeposedRefreshTarget {
    row_provider: String,
    row_resource_type: String,
    row_identity: String,
    row_provider_instance: Option<String>,
    id: ResourceId,
    key: DeposedKey,
    identifier: String,
    provider_instance: Option<String>,
    attributes: HashMap<String, serde_json::Value>,
    dependency_bindings: BTreeSet<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DeposedRefreshSummary {
    total_generations: u32,
    removed_generations: u32,
    updated_generations: u32,
    failed_generations: u32,
}

async fn refresh_deposed_generations_until_cancelled<P>(
    provider: &P,
    state: &mut carina_state::StateFile,
    desired_resources: &[Resource],
    schemas: &carina_core::schema::SchemaRegistry,
    cancel: &ShutdownToken,
) -> Result<DeposedRefreshSummary, AppError>
where
    P: Provider + ProviderNormalizer + ?Sized,
{
    let targets = collect_deposed_refresh_targets(state);
    let mut summary = DeposedRefreshSummary {
        total_generations: targets.len() as u32,
        removed_generations: 0,
        updated_generations: 0,
        failed_generations: 0,
    };

    for target in targets {
        if shutdown_requested(cancel) {
            return Err(AppError::Interrupted);
        }

        let read_result = tokio::select! {
            biased;
            _ = cancel.cleanup_priority_requested() => return Err(AppError::Interrupted),
            result = provider.read(
                &target.id,
                Some(target.identifier.as_str()),
                carina_core::provider::ReadRequest,
            ) => result,
        };
        if shutdown_requested(cancel) {
            return Err(AppError::Interrupted);
        }

        let fresh_state = match read_result {
            Ok(fresh_state) => fresh_state,
            Err(err) => {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: failed refreshing {}.{} {} {}: {}",
                        target.row_provider,
                        target.row_resource_type,
                        target.row_identity,
                        deposed_state_marker(&target.key, &target.identifier),
                        err
                    )
                    .yellow()
                );
                summary.failed_generations += 1;
                continue;
            }
        };

        if !fresh_state.exists {
            state.remove_deposed_generation(
                &target.row_provider,
                &target.row_resource_type,
                &target.row_identity,
                &target.key,
            );
            summary.removed_generations += 1;
            println!(
                "  {} \"{}\" {}:",
                target.id.display_type().cyan(),
                target.id.identity_or_empty(),
                deposed_state_marker(&target.key, &target.identifier)
            );
            println!("    {} resource no longer exists", "-".red());
            println!();
            continue;
        }

        let masking_resource = desired_resource_for_deposed(desired_resources, &target)
            .cloned()
            .unwrap_or_else(|| synthetic_deposed_resource(&target));
        let mut fresh_state = fresh_state;
        normalize_deposed_read_state(
            provider,
            &target,
            &masking_resource,
            &mut fresh_state,
            schemas,
        )
        .await;
        if shutdown_requested(cancel) {
            return Err(AppError::Interrupted);
        }
        let schema = schemas.get_for(&masking_resource);
        let attributes = ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
            &masking_resource,
            schema,
            &fresh_state.attributes,
            carina_state::PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&target.attributes),
        );

        if attributes == target.attributes {
            continue;
        }
        summary.updated_generations += 1;

        let updated = DeposedInstance {
            key: target.key.clone(),
            identifier: target.identifier.clone(),
            provider_instance: target.provider_instance.clone(),
            attributes,
            dependency_bindings: target.dependency_bindings.clone(),
        };
        state.upsert_deposed_generation(
            &target.row_provider,
            &target.row_resource_type,
            &target.row_identity,
            target.row_provider_instance.clone(),
            updated,
        )?;
        println!(
            "  {} \"{}\" {}:",
            target.id.display_type().cyan(),
            target.id.identity_or_empty(),
            deposed_state_marker(&target.key, &target.identifier)
        );
        println!("    {} attributes refreshed", "~".yellow());
        println!();
    }

    Ok(summary)
}

fn collect_deposed_refresh_targets(state: &carina_state::StateFile) -> Vec<DeposedRefreshTarget> {
    state
        .resources()
        .iter()
        .flat_map(|row| {
            row.deposed.iter().map(|deposed| {
                let id = ResourceId::with_provider_name_compat(
                    &row.provider,
                    &row.resource_type,
                    &row.identity,
                    deposed.provider_instance.clone(),
                );
                DeposedRefreshTarget {
                    row_provider: row.provider.clone(),
                    row_resource_type: row.resource_type.clone(),
                    row_identity: row.identity.clone(),
                    row_provider_instance: row.directives.provider_instance.clone(),
                    id,
                    key: deposed.key.clone(),
                    identifier: deposed.identifier.clone(),
                    provider_instance: deposed.provider_instance.clone(),
                    attributes: deposed.attributes.clone(),
                    dependency_bindings: deposed.dependency_bindings.clone(),
                }
            })
        })
        .collect()
}

fn desired_resource_for_deposed<'a>(
    desired_resources: &'a [Resource],
    target: &DeposedRefreshTarget,
) -> Option<&'a Resource> {
    desired_resources.iter().find(|resource| {
        resource.id.provider == target.row_provider
            && resource.id.resource_type == target.row_resource_type
            && resource.id.identity_or_empty() == target.row_identity
            && resource.id.provider_instance == target.provider_instance
    })
}

fn synthetic_deposed_resource(target: &DeposedRefreshTarget) -> Resource {
    let mut resource = Resource::with_provider(
        &target.row_provider,
        &target.row_resource_type,
        &target.row_identity,
        target.provider_instance.clone(),
    );
    for (key, value) in &target.attributes {
        if let Some(dsl_value) = json_to_dsl_value(value) {
            resource.set_attr(key.clone(), dsl_value);
        }
    }
    resource
}

async fn normalize_deposed_read_state<P>(
    provider: &P,
    target: &DeposedRefreshTarget,
    resource: &Resource,
    fresh_state: &mut State,
    schemas: &carina_core::schema::SchemaRegistry,
) where
    P: ProviderNormalizer + ?Sized,
{
    let mut states = HashMap::from([(target.id.clone(), fresh_state.clone())]);
    let resources = std::slice::from_ref(resource);
    let saved_attrs = deposed_saved_attrs(target).lift(schemas);
    provider
        .hydrate_read_state(&mut states, saved_attrs.as_provider_saved_attrs())
        .await;
    carina_core::utils::lift_current_state_enum_leaves(&mut states, resources, schemas);

    if let Some(normalized) = states.remove(&target.id) {
        *fresh_state = normalized;
    }
}

fn deposed_saved_attrs(target: &DeposedRefreshTarget) -> RawSavedAttrs {
    let attrs = target
        .attributes
        .iter()
        .filter_map(|(key, value)| json_to_dsl_value(value).map(|dsl| (key.clone(), dsl)))
        .collect();
    RawSavedAttrs::from_persisted(HashMap::from([(target.id.clone(), attrs)]))
}

/// Compare old state with fresh provider state for a single resource,
/// display any changes, and update the state file accordingly.
///
/// When `resource` is `Some`, directives, prefixes, and desired keys
/// are preserved from it. When `None` (orphan resources), a minimal
/// `Resource` is constructed from the id.
///
/// `label_suffix` is appended to the resource header (e.g., `" (orphan)"`).
#[derive(Default)]
struct StateRefreshCounts {
    updated: u32,
    unchanged: u32,
}

fn diff_display_update_resource(
    id: &ResourceId,
    fresh_state: &State,
    state: &mut carina_state::StateFile,
    resource: Option<&Resource>,
    schemas: &carina_core::schema::SchemaRegistry,
    label_suffix: &str,
    counts: &mut StateRefreshCounts,
) -> Result<(), AppError> {
    let existing = state.find_resource(&id.provider, &id.resource_type, id.identity_or_empty());
    let existing_rs = match existing {
        Some(rs) => rs,
        None => return Ok(()),
    };

    let refreshed_resource_state = if fresh_state.exists {
        let owned_resource;
        let res = match resource {
            Some(r) => r,
            None => {
                owned_resource = Resource::with_provider(
                    &id.provider,
                    &id.resource_type,
                    id.identity_or_empty(),
                    id.provider_instance.clone(),
                );
                &owned_resource
            }
        };
        let schema = schemas.get_for(res);
        let mut resource_state = ResourceState::from_provider_state_for_resource_and_schema(
            res,
            fresh_state,
            Some(existing_rs),
            schema,
        )?;
        if let Some(resource) = resource {
            let write_only_keys: Vec<String> = schema
                .map(|schema| {
                    schema
                        .attributes
                        .iter()
                        .filter(|(_, attr)| attr.write_only)
                        .map(|(name, _)| name.clone())
                        .collect()
                })
                .unwrap_or_default();
            if !write_only_keys.is_empty() {
                resource_state.merge_write_only_attributes(resource, &write_only_keys);
            }
        }
        Some(resource_state)
    } else {
        None
    };

    // Build old attributes as DSL values for comparison
    let old_attrs: HashMap<String, Value> = existing_rs
        .attributes
        .iter()
        .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
        .collect();
    let refreshed_attrs: HashMap<String, Value> = refreshed_resource_state
        .as_ref()
        .map(|rs| {
            rs.attributes
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect()
        })
        .unwrap_or_default();

    let mut has_changes = false;
    let mut changes: Vec<String> = Vec::new();

    if !fresh_state.exists {
        // Resource was deleted externally
        has_changes = true;
        changes.push(format!("    {} resource no longer exists", "-".red()));
    } else {
        // Check for modified, added, and removed attributes
        let mut all_keys: HashSet<&String> = old_attrs.keys().collect();
        all_keys.extend(refreshed_attrs.keys());

        let mut sorted_keys: Vec<&&String> = all_keys.iter().collect();
        sorted_keys.sort();

        for key in sorted_keys {
            let old_val = old_attrs.get(*key);
            let new_val = refreshed_attrs.get(*key);

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
        counts.updated += 1;
        println!(
            "  {} \"{}\"{}:",
            id.display_type().cyan(),
            id.identity_or_empty(),
            label_suffix,
        );
        for change in &changes {
            println!("{}", change);
        }
        println!();
    } else {
        counts.unchanged += 1;
    }

    // Update state with refreshed data
    if let Some(resource_state) = refreshed_resource_state {
        state.upsert_resource(resource_state)?;
    } else {
        state.remove_resource(&id.provider, &id.resource_type, id.identity_or_empty());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::parser::parse;
    use carina_core::provider::{
        BoxFuture, CreateRequest, DeleteRequest, ProviderError, ProviderResult, ReadRequest,
        UpdateRequest,
    };
    use carina_core::resource::DeferredValue;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    use carina_core::value::SECRET_PREFIX;
    use carina_state::{DeposedInstance, DeposedKey};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Default)]
    struct DeposedRefreshTestProvider {
        read_results: HashMap<(String, String), Result<State, String>>,
        hydrate_saved_attrs: bool,
        hydrated_saved_values: Mutex<Vec<Value>>,
    }

    impl DeposedRefreshTestProvider {
        fn with_read_state(mut self, id: &ResourceId, identifier: &str, state: State) -> Self {
            self.read_results
                .insert((id.to_string(), identifier.to_string()), Ok(state));
            self
        }

        fn with_read_error(
            mut self,
            id: &ResourceId,
            identifier: &str,
            error: impl Into<String>,
        ) -> Self {
            self.read_results
                .insert((id.to_string(), identifier.to_string()), Err(error.into()));
            self
        }

        fn with_saved_attr_hydration(mut self) -> Self {
            self.hydrate_saved_attrs = true;
            self
        }
    }

    impl Provider for DeposedRefreshTestProvider {
        fn name(&self) -> &str {
            "deposed-refresh-test"
        }

        fn read(
            &self,
            id: &ResourceId,
            identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let key = (id.to_string(), identifier.unwrap_or("").to_string());
            let result = self
                .read_results
                .get(&key)
                .cloned()
                .unwrap_or_else(|| panic!("missing deposed refresh result for {:?}", key));
            Box::pin(async move { result.map_err(ProviderError::internal) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::resource::DataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.read(&resource.id, None, ReadRequest)
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unexpected create")) })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unexpected update")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    impl ProviderNormalizer for DeposedRefreshTestProvider {
        fn normalize_desired<'a>(&'a self, _resources: &'a mut [Resource]) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn hydrate_read_state<'a>(
            &'a self,
            current_states: &'a mut HashMap<ResourceId, State>,
            saved_attrs: &'a carina_core::provider::SavedAttrs,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                if !self.hydrate_saved_attrs {
                    return;
                }
                for (id, state) in current_states.iter_mut() {
                    let Some(saved) = saved_attrs.get(id) else {
                        continue;
                    };
                    for (key, value) in saved {
                        state
                            .attributes
                            .entry(key.clone())
                            .or_insert_with(|| value.clone());
                        self.hydrated_saved_values
                            .lock()
                            .expect("hydration probe lock")
                            .push(value.clone());
                    }
                }
            })
        }

        fn merge_default_tags<'a>(
            &'a self,
            _resources: &'a mut [Resource],
            _default_tags: &'a indexmap::IndexMap<String, Value>,
            _registry: &'a SchemaRegistry,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    fn deposed_key(raw: &str) -> DeposedKey {
        serde_json::from_value(json!(raw)).expect("test deposed key should deserialize")
    }

    fn deposed_instance(
        key: DeposedKey,
        identifier: &str,
        provider_instance: Option<&str>,
        attributes: HashMap<String, serde_json::Value>,
    ) -> DeposedInstance {
        DeposedInstance {
            key,
            identifier: identifier.to_string(),
            provider_instance: provider_instance.map(str::to_string),
            attributes,
            dependency_bindings: BTreeSet::new(),
        }
    }

    fn string_value(raw: &str) -> Value {
        Value::Concrete(ConcreteValue::String(raw.to_string()))
    }

    #[tokio::test]
    async fn state_refresh_expansion_hydrates_lifted_saved_enum_attrs() {
        let parsed = parse(
            r#"
                let source = aws.service.Source {
                    name = "source"
                }

                for (_, item) in source.items {
                    aws.service.Widget {
                        name = item
                    }
                }
            "#,
            &ProviderContext::default(),
        )
        .expect("test config should parse");
        assert_eq!(parsed.deferred_for_expressions.len(), 1);

        let sorted_resources = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let source = parsed
            .resources
            .iter()
            .find(|resource| resource.binding.as_deref() == Some("source"))
            .expect("source resource");
        let mut current_states = HashMap::from([(
            source.id.clone(),
            State::existing(
                source.id.clone(),
                HashMap::from([(
                    "items".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![string_value("widget-1")])),
                )]),
            ),
        )]);

        let preview = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted_resources,
            &current_states,
            &SchemaRegistry::new(),
            &HashMap::new(),
            &[],
            &HashSet::new(),
            &HashSet::new(),
        )
        .expect("loop should expand");
        let child = preview
            .sorted_resources
            .iter()
            .find(|resource| resource.id.resource_type == "service.Widget")
            .expect("materialized child")
            .clone();
        assert_eq!(preview.new_child_ids, HashSet::from([child.id.clone()]));

        let mut state_file = StateFile::new();
        let mut child_state = ResourceState::new(
            &child.id.resource_type,
            child.id.identity_or_empty(),
            &child.id.provider,
        )
        .with_identifier("widget-1")
        .with_attribute("status", json!("Enabled"));
        child_state.directives.provider_instance = child.id.provider_instance.clone();
        state_file
            .upsert_resource(child_state)
            .expect("test state setup must be valid");

        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            ResourceSchema::new("service.Widget").attribute(AttributeSchema::new(
                "status",
                AttributeType::enum_(
                    carina_core::schema::enum_identity("Status", Some("aws.service.Widget")),
                    Some(vec!["Enabled".to_string()]),
                    vec![("Enabled".to_string(), "enabled".to_string())],
                    None,
                    None,
                ),
            )),
        );

        // This is the saved-attribute construction used by
        // `run_state_refresh_locked` before the shared expansion path.
        let saved_attrs_for_expansion = state_file.build_saved_attrs().lift(&schemas);
        let state_file = Some(state_file);
        let provider = DeposedRefreshTestProvider::default()
            .with_saved_attr_hydration()
            .with_read_state(
                &child.id,
                "widget-1",
                State::existing(child.id.clone(), HashMap::new()).with_identifier("widget-1"),
            );
        let multi =
            indicatif::MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());

        let expanded = crate::wiring::expand_refresh_and_lift_states(
            crate::wiring::ExpandRefreshAndLiftInputs {
                parsed: &parsed,
                provider: &provider,
                sorted_resources: &sorted_resources,
                current_states: &mut current_states,
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
                moved_targets: &HashSet::new(),
                already_refreshed: &HashSet::new(),
                state_file: &state_file,
                saved_dep_bindings: &HashMap::new(),
                saved_attrs: &saved_attrs_for_expansion,
                multi: &multi,
                schemas: &schemas,
            },
        )
        .await
        .expect("refresh expansion should succeed");
        assert_eq!(expanded.new_child_ids, HashSet::from([child.id.clone()]));

        let hydrated = provider
            .hydrated_saved_values
            .lock()
            .expect("hydration probe lock");
        assert!(
            matches!(
                hydrated.as_slice(),
                [Value::Concrete(ConcreteValue::CanonicalEnum(value))]
                    if value.api_value() == "Enabled"
            ),
            "saved enum must be lifted before hydration, got {hydrated:?}"
        );
    }

    fn deposed_format_state() -> StateFile {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-new")
            .with_attribute("cidr_block", json!("10.0.0.0/16"))
            .with_attribute("vpc_id", json!("vpc-new"));
        row.binding = Some("main".to_string());
        row.deposed.push(deposed_instance(
            deposed_key("dep-a"),
            "vpc-old",
            None,
            HashMap::from([
                ("cidr_block".to_string(), json!("10.0.1.0/16")),
                ("vpc_id".to_string(), json!("vpc-old")),
            ]),
        ));
        row.deposed.push(deposed_instance(
            deposed_key("dep-b"),
            "vpc-older",
            None,
            HashMap::from([("vpc_id".to_string(), json!("vpc-older"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let mut shell = ResourceState::new("ec2.Subnet", "abandoned", "awscc");
        shell.binding = Some("abandoned".to_string());
        shell.deposed.push(deposed_instance(
            deposed_key("dep-shell"),
            "subnet-old",
            None,
            HashMap::from([("subnet_id".to_string(), json!("subnet-old"))]),
        ));
        state
            .upsert_resource(shell)
            .expect("test state setup must be valid");

        state
    }

    /// Load the fixture state file from `tests/fixtures/state_lookup/`.
    fn load_fixture_state() -> StateFile {
        load_fixture("state_lookup")
    }

    /// Load a named fixture state file from `tests/fixtures/<name>/`.
    fn load_fixture(name: &str) -> StateFile {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = PathBuf::from(format!(
            "{}/tests/fixtures/{}/carina.state.json",
            manifest_dir, name
        ));
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", path.display(), e));
        carina_state::check_and_migrate(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path.display(), e))
            .into_state()
    }

    // --- find_resource_by_query tests ---

    #[test]
    fn find_resource_by_binding() {
        let state = load_fixture_state();
        let found = find_resource_by_query(&state, "vpc").unwrap();
        assert_eq!(found.identity, "my-vpc");
        assert_eq!(found.resource_type, "ec2.Vpc");
    }

    #[test]
    fn find_resource_by_name_fallback() {
        let state = load_fixture_state();
        // "main-rt" has no binding, so lookup by name should work
        let found = find_resource_by_query(&state, "main-rt").unwrap();
        assert_eq!(found.resource_type, "ec2.RouteTable");
    }

    #[test]
    fn find_resource_not_found() {
        let state = load_fixture_state();
        assert!(find_resource_by_query(&state, "nonexistent").is_none());
    }

    #[test]
    fn binding_takes_precedence_over_name() {
        let mut state = StateFile::new();
        let mut rs1 = ResourceState::new("ec2.Vpc", "vpc", "awscc");
        rs1.binding = Some("my_vpc".to_string());
        let mut rs2 = ResourceState::new("ec2.Subnet", "my_vpc", "awscc");
        rs2.binding = None;
        state
            .upsert_resource(rs1)
            .expect("test state setup must be valid");
        state
            .upsert_resource(rs2)
            .expect("test state setup must be valid");

        let found = find_resource_by_query(&state, "my_vpc").unwrap();
        // Should find the one with binding="my_vpc", not name="my_vpc"
        assert_eq!(found.resource_type, "ec2.Vpc");
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

    #[test]
    fn state_list_includes_deposed_entries_and_shell_rows() {
        let lines = format_state_list(&deposed_format_state());
        assert_eq!(
            lines,
            vec![
                "awscc.ec2.Vpc main",
                "awscc.ec2.Vpc main  (deposed dep-a vpc-old)",
                "awscc.ec2.Vpc main  (deposed dep-b vpc-older)",
                "awscc.ec2.Subnet abandoned  (no current instance)",
                "awscc.ec2.Subnet abandoned  (deposed dep-shell subnet-old)",
            ]
        );
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

    #[test]
    fn lookup_full_resource_includes_deposed_generations() {
        let output = format_state_lookup(&deposed_format_state(), "main", false).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["current"]["identifier"], "vpc-new");
        assert_eq!(parsed["current"]["attributes"]["vpc_id"], "vpc-new");
        assert_eq!(parsed["deposed"][0]["key"], "dep-a");
        assert_eq!(parsed["deposed"][0]["identifier"], "vpc-old");
        assert_eq!(parsed["deposed"][0]["marker"], "(deposed dep-a vpc-old)");
        assert_eq!(parsed["deposed"][0]["attributes"]["vpc_id"], "vpc-old");
        assert_eq!(parsed["deposed"][1]["key"], "dep-b");
        assert_eq!(parsed["deposed"][1]["identifier"], "vpc-older");
    }

    #[test]
    fn lookup_attribute_ignores_deposed_generations() {
        let output = format_state_lookup(&deposed_format_state(), "main.vpc_id", false).unwrap();
        assert_eq!(output, "vpc-new");

        let output = format_state_lookup(&deposed_format_state(), "main.vpc_id", true).unwrap();
        assert_eq!(output, "\"vpc-new\"");
    }

    #[test]
    fn lookup_attribute_missing_on_current_errors_even_when_deposed_has_it() {
        let err = format_state_lookup(&deposed_format_state(), "abandoned.subnet_id", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Attribute 'subnet_id' not found on resource 'abandoned'"),
            "unexpected error: {err}"
        );

        let err = format_state_lookup(&deposed_format_state(), "abandoned.subnet_id", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Attribute 'subnet_id' not found on resource 'abandoned'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn lookup_shell_row_returns_deposed_generations() {
        let output = format_state_lookup(&deposed_format_state(), "abandoned", false).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed["current"].is_null());
        assert_eq!(parsed["deposed"][0]["key"], "dep-shell");
        assert_eq!(parsed["deposed"][0]["identifier"], "subnet-old");
        assert_eq!(
            parsed["deposed"][0]["marker"],
            "(deposed dep-shell subnet-old)"
        );
        assert_eq!(
            parsed["deposed"][0]["attributes"]["subnet_id"],
            "subnet-old"
        );
    }

    #[test]
    fn lookup_full_resource_current_keeps_attributes_without_identifier() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_attribute("vpc_id", json!("vpc-current"));
        row.binding = Some("main".to_string());
        row.deposed.push(deposed_instance(
            deposed_key("dep-a"),
            "vpc-old",
            None,
            HashMap::from([("vpc_id".to_string(), json!("vpc-old"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let output = format_state_lookup(&state, "main", false).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed["current"]["identifier"].is_null());
        assert_eq!(parsed["current"]["attributes"]["vpc_id"], "vpc-current");
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
    fn completion_future_state_version_is_rejected_without_candidates() {
        let found = StateFile::CURRENT_VERSION + 1;
        let contents = json!({
            "version": found,
            "serial": 0,
            "lineage": "future-completion-test",
            "carina_version": "future",
            "resources": []
        })
        .to_string();

        let result = parse_local_state_for_completion(&contents);
        assert!(
            matches!(
                result.as_ref(),
                Err(BackendError::StateVersionTooNew {
                    found: actual_found,
                    supported,
                }) if *actual_found == found && *supported == StateFile::CURRENT_VERSION
            ),
            "completion must preserve the typed future-version classification"
        );
    }

    #[test]
    fn completion_migrates_v8_state_before_producing_candidates() {
        let contents = json!({
            "version": 8,
            "serial": 3,
            "lineage": "v8-completion-test",
            "carina_version": "0.4.0",
            "resources": [{
                "resource_type": "ec2.Vpc",
                "identity": "main-vpc",
                "provider": "awscc",
                "identifier": "vpc-123",
                "attributes": { "vpc_id": "vpc-123" },
                "binding": "vpc"
            }]
        })
        .to_string();

        let state = parse_local_state_for_completion(&contents)
            .expect("older state should migrate for completion");
        assert_eq!(state.version, StateFile::CURRENT_VERSION);
        assert_eq!(
            candidate_values(&complete_state_lookup_from(&state, "v")),
            vec!["vpc"]
        );
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

    #[test]
    fn completion_does_not_offer_deposed_only_attributes() {
        let candidates = complete_state_lookup_from(&deposed_format_state(), "abandoned.");
        assert!(candidates.is_empty());
    }

    // --- format_state_show tests ---

    #[test]
    fn state_show_displays_all_resources_with_attributes() {
        let state = load_fixture_state();
        let output = format_state_show(&state);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn state_show_includes_deposed_generations_and_shell_rows() {
        let output = format_state_show(&deposed_format_state());
        assert_eq!(
            output,
            "# awscc.ec2.Vpc (main)\n  cidr_block = \"10.0.0.0/16\"\n  vpc_id = \"vpc-new\"\n  (deposed dep-a vpc-old)\n    cidr_block = \"10.0.1.0/16\"\n    vpc_id = \"vpc-old\"\n  (deposed dep-b vpc-older)\n    vpc_id = \"vpc-older\"\n\n# awscc.ec2.Subnet (abandoned)\n  (deposed dep-shell subnet-old)\n    subnet_id = \"subnet-old\"\n"
        );
    }

    #[test]
    fn state_refresh_summary_reports_deposed_generations_separately() {
        assert_eq!(
            format_state_refresh_summary(
                2,
                1,
                &DeposedRefreshSummary {
                    total_generations: 3,
                    removed_generations: 1,
                    updated_generations: 2,
                    failed_generations: 0,
                },
            ),
            "State refreshed: 2 resources updated, 1 resource unchanged, 3 deposed generations reconciled (1 removed, 2 updated, 0 unchanged)."
        );
        assert_eq!(
            format_state_refresh_summary(
                1,
                2,
                &DeposedRefreshSummary {
                    total_generations: 1,
                    removed_generations: 1,
                    updated_generations: 0,
                    failed_generations: 0,
                },
            ),
            "State refreshed: 1 resource updated, 2 resources unchanged, 1 deposed generation reconciled (1 removed, 0 updated, 0 unchanged)."
        );
        assert_eq!(
            format_state_refresh_summary(
                1,
                2,
                &DeposedRefreshSummary {
                    total_generations: 3,
                    removed_generations: 1,
                    updated_generations: 1,
                    failed_generations: 1,
                },
            ),
            "State refreshed: 1 resource updated, 2 resources unchanged, 2 deposed generations reconciled (1 removed, 1 updated, 0 unchanged), 1 failed."
        );
        assert_eq!(
            format_state_refresh_summary(
                0,
                1,
                &DeposedRefreshSummary {
                    total_generations: 2,
                    removed_generations: 0,
                    updated_generations: 0,
                    failed_generations: 0,
                },
            ),
            "State refreshed: 0 resources updated, 1 resource unchanged, 0 deposed generations reconciled (0 removed, 0 updated, 2 unchanged)."
        );
        assert_eq!(
            format_state_refresh_summary(1, 2, &DeposedRefreshSummary::default()),
            "State refreshed: 1 resource updated, 2 resources unchanged."
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generations_drops_gone_updates_alive_and_preserves_current() {
        let gone_key = deposed_key("gone-key");
        let alive_key = deposed_key("alive-key");
        let mut state = StateFile::new();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-current")
            .with_attribute("vpc_id", json!("vpc-current"))
            .with_attribute("cidr_block", json!("10.0.0.0/16"));
        row.deposed.push(deposed_instance(
            gone_key.clone(),
            "vpc-gone",
            None,
            HashMap::from([("vpc_id".to_string(), json!("vpc-gone"))]),
        ));
        row.deposed.push(deposed_instance(
            alive_key.clone(),
            "vpc-alive",
            Some("west"),
            HashMap::from([("vpc_id".to_string(), json!("vpc-alive-old"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let gone_id = ResourceId::with_provider_name_compat("awscc", "ec2.Vpc", "main", None);
        let alive_id = ResourceId::with_provider_name_compat(
            "awscc",
            "ec2.Vpc",
            "main",
            Some("west".to_string()),
        );
        let provider = DeposedRefreshTestProvider::default()
            .with_read_state(&gone_id, "vpc-gone", State::not_found(gone_id.clone()))
            .with_read_state(
                &alive_id,
                "vpc-alive",
                State::existing(
                    alive_id.clone(),
                    HashMap::from([
                        ("vpc_id".to_string(), string_value("vpc-alive")),
                        ("cidr_block".to_string(), string_value("10.0.1.0/16")),
                    ]),
                )
                .with_identifier("vpc-alive"),
            );
        let desired = Resource::with_provider("awscc", "ec2.Vpc", "main", None);

        let summary = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[desired],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let row = state.find_resource("awscc", "ec2.Vpc", "main").unwrap();
        assert_eq!(row.identifier.as_deref(), Some("vpc-current"));
        assert_eq!(row.attributes.get("vpc_id"), Some(&json!("vpc-current")));
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].key, alive_key);
        assert_eq!(row.deposed[0].identifier, "vpc-alive");
        assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
        assert_eq!(
            row.deposed[0].attributes.get("vpc_id"),
            Some(&json!("vpc-alive"))
        );
        assert_eq!(
            row.deposed[0].attributes.get("cidr_block"),
            Some(&json!("10.0.1.0/16"))
        );
        assert_eq!(
            summary,
            DeposedRefreshSummary {
                total_generations: 2,
                removed_generations: 1,
                updated_generations: 1,
                failed_generations: 0,
            }
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_removes_shell_row_when_gone() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("gone-key"),
            "vpc-gone",
            None,
            HashMap::from([("vpc_id".to_string(), json!("vpc-gone"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "ec2.Vpc", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "vpc-gone",
            State::not_found(id.clone()),
        );

        let summary = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        assert!(state.find_resource("awscc", "ec2.Vpc", "main").is_none());
        assert_eq!(
            summary,
            DeposedRefreshSummary {
                total_generations: 1,
                removed_generations: 1,
                updated_generations: 0,
                failed_generations: 0,
            }
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_masks_plaintext_secret_from_provider_read() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("secret-key"),
            "db-old",
            None,
            HashMap::from([("password".to_string(), json!("old-hash"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "db-old",
            State::existing(
                id.clone(),
                HashMap::from([("password".to_string(), string_value("plain-secret"))]),
            )
            .with_identifier("db-old"),
        );
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
            "password",
            Value::Deferred(DeferredValue::Secret(Box::new(string_value(
                "plain-secret",
            )))),
        );

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[desired],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let stored = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .get("password")
            .and_then(|value| value.as_str())
            .expect("password should be stored as a string hash");
        assert!(stored.starts_with(SECRET_PREFIX), "got {stored}");
        assert!(!stored.contains("plain-secret"));
    }

    #[tokio::test]
    async fn refresh_deposed_generation_drops_schema_write_only_plaintext_without_desired_secret() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("secret-key"),
            "db-old",
            None,
            HashMap::from([("password".to_string(), json!("old-hash"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "db-old",
            State::existing(
                id.clone(),
                HashMap::from([
                    ("password".to_string(), string_value("plain-secret")),
                    ("endpoint".to_string(), string_value("db.example")),
                ]),
            )
            .with_identifier("db-old"),
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            ResourceSchema::new("db.Instance")
                .attribute(AttributeSchema::new("password", AttributeType::string()).write_only())
                .attribute(AttributeSchema::new("endpoint", AttributeType::string())),
        );

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let attributes = &state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .attributes;
        assert_eq!(attributes.get("endpoint"), Some(&json!("db.example")));
        assert!(
            !attributes.contains_key("password"),
            "schema write_only provider plaintext must not be persisted"
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_rehashes_existing_hash_without_desired_secret() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("secret-key"),
            "db-old",
            None,
            HashMap::from([(
                "password".to_string(),
                json!(format!("{SECRET_PREFIX}previous")),
            )]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "db-old",
            State::existing(
                id.clone(),
                HashMap::from([("password".to_string(), string_value("plain-secret"))]),
            )
            .with_identifier("db-old"),
        );

        let summary = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let stored = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .get("password")
            .and_then(|value| value.as_str())
            .expect("password should remain a stored secret hash");
        assert!(stored.starts_with(SECRET_PREFIX), "got {stored}");
        assert!(!stored.contains("plain-secret"));
        assert_eq!(
            summary,
            DeposedRefreshSummary {
                total_generations: 1,
                removed_generations: 0,
                updated_generations: 1,
                failed_generations: 0,
            }
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_merges_nested_secret_hash_per_leaf_and_converges() {
        let mut state = StateFile::new();
        let previous_tags = json!({
            "Name": "old-name",
            "SecretTag": format!("{SECRET_PREFIX}previous"),
        });
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("nested-secret-key"),
            "vpc-old",
            None,
            HashMap::from([("tags".to_string(), previous_tags.clone())]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "ec2.Vpc", "main", None);
        let mut provider_tags = indexmap::IndexMap::new();
        provider_tags.insert("Name".to_string(), string_value("new-name"));
        provider_tags.insert("SecretTag".to_string(), string_value("plain-secret"));
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "vpc-old",
            State::existing(
                id.clone(),
                HashMap::from([(
                    "tags".to_string(),
                    Value::Concrete(ConcreteValue::Map(provider_tags)),
                )]),
            )
            .with_identifier("vpc-old"),
        );

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let stored_tags = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .get("tags")
            .cloned()
            .expect("tags should be stored");
        assert_eq!(stored_tags["Name"], json!("new-name"));
        let secret = stored_tags["SecretTag"]
            .as_str()
            .expect("secret tag should stay a string");
        assert!(secret.starts_with(SECRET_PREFIX), "got {secret}");
        assert!(!secret.contains("plain-secret"));

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();
        let stored_again = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .get("tags")
            .cloned()
            .expect("tags should still be stored");
        assert_eq!(stored_again, stored_tags);
    }

    #[test]
    fn current_orphan_refresh_rehashes_existing_hash_without_desired_secret() {
        let id = ResourceId::with_provider_name_compat("awscc", "db.Instance", "main", None);
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::new("db.Instance", "main", "awscc")
                    .with_identifier("db-current")
                    .with_attribute("password", json!(format!("{SECRET_PREFIX}previous"))),
            )
            .expect("test state setup must be valid");
        let fresh = State::existing(
            id.clone(),
            HashMap::from([("password".to_string(), string_value("plain-secret"))]),
        )
        .with_identifier("db-current");
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &id,
            &fresh,
            &mut state,
            None,
            &SchemaRegistry::new(),
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let stored = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .attributes
            .get("password")
            .and_then(|value| value.as_str())
            .expect("password should remain a stored secret hash");
        assert!(stored.starts_with(SECRET_PREFIX), "got {stored}");
        assert!(!stored.contains("plain-secret"));
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 0);
    }

    #[test]
    fn current_orphan_refresh_merges_nested_secret_hash_per_leaf_and_converges() {
        let id = ResourceId::with_provider_name_compat("awscc", "ec2.Vpc", "main", None);
        let previous_tags = json!({
            "Name": "old-name",
            "SecretTag": format!("{SECRET_PREFIX}previous"),
        });
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::new("ec2.Vpc", "main", "awscc")
                    .with_identifier("vpc-current")
                    .with_attribute("tags", previous_tags),
            )
            .expect("test state setup must be valid");
        let mut provider_tags = indexmap::IndexMap::new();
        provider_tags.insert("Name".to_string(), string_value("new-name"));
        provider_tags.insert("SecretTag".to_string(), string_value("plain-secret"));
        let fresh = State::existing(
            id.clone(),
            HashMap::from([(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(provider_tags)),
            )]),
        )
        .with_identifier("vpc-current");
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &id,
            &fresh,
            &mut state,
            None,
            &SchemaRegistry::new(),
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let stored_tags = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .attributes
            .get("tags")
            .cloned()
            .expect("tags should be stored");
        assert_eq!(stored_tags["Name"], json!("new-name"));
        let secret = stored_tags["SecretTag"]
            .as_str()
            .expect("secret tag should stay a string");
        assert!(secret.starts_with(SECRET_PREFIX), "got {secret}");
        assert!(!secret.contains("plain-secret"));
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 0);

        diff_display_update_resource(
            &id,
            &fresh,
            &mut state,
            None,
            &SchemaRegistry::new(),
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let stored_again = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .attributes
            .get("tags")
            .cloned()
            .expect("tags should still be stored");
        assert_eq!(stored_again, stored_tags);
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 1);
    }

    #[test]
    fn current_refresh_drops_schema_write_only_plaintext_absent_from_desired() {
        let id = ResourceId::with_provider_name_compat("awscc", "db.Instance", "main", None);
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None);
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::new("db.Instance", "main", "awscc").with_identifier("db-current"),
            )
            .expect("test state setup must be valid");
        let fresh = State::existing(
            id.clone(),
            HashMap::from([("password".to_string(), string_value("plain-secret"))]),
        )
        .with_identifier("db-current");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            ResourceSchema::new("db.Instance")
                .attribute(AttributeSchema::new("password", AttributeType::string()).write_only()),
        );
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &id,
            &fresh,
            &mut state,
            Some(&desired),
            &schemas,
            "",
            &mut counts,
        )
        .unwrap();

        let row = state.find_resource("awscc", "db.Instance", "main").unwrap();
        assert!(
            !row.attributes.contains_key("password"),
            "write-only plaintext must not be persisted by refresh"
        );
        assert_eq!(counts.updated, 0);
        assert_eq!(counts.unchanged, 1);
    }

    #[tokio::test]
    async fn refresh_deposed_generation_hydrates_unreturned_attributes() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("hydrate-key"),
            "db-old",
            None,
            HashMap::from([
                ("description".to_string(), json!("kept from state")),
                ("endpoint".to_string(), json!("old.example")),
            ]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default()
            .with_saved_attr_hydration()
            .with_read_state(
                &id,
                "db-old",
                State::existing(
                    id.clone(),
                    HashMap::from([("endpoint".to_string(), string_value("new.example"))]),
                )
                .with_identifier("db-old"),
            );

        let summary = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let attrs = &state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .attributes;
        assert_eq!(attrs.get("endpoint"), Some(&json!("new.example")));
        assert_eq!(attrs.get("description"), Some(&json!("kept from state")));
        assert_eq!(
            summary,
            DeposedRefreshSummary {
                total_generations: 1,
                removed_generations: 0,
                updated_generations: 1,
                failed_generations: 0,
            }
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_lifts_enum_state_and_is_stable() {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            ResourceSchema::new("service.Widget").attribute(AttributeSchema::new(
                "status",
                AttributeType::enum_(
                    carina_core::schema::enum_identity("Status", Some("awscc.service.Widget")),
                    Some(vec!["Enabled".to_string()]),
                    vec![("Enabled".to_string(), "enabled".to_string())],
                    None,
                    None,
                ),
            )),
        );

        let mut state = StateFile::new();
        let mut row = ResourceState::new("service.Widget", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("enum-key"),
            "widget-old",
            None,
            HashMap::from([("status".to_string(), json!("Enabled"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "service.Widget", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "widget-old",
            State::existing(
                id.clone(),
                HashMap::from([("status".to_string(), string_value("Enabled"))]),
            )
            .with_identifier("widget-old"),
        );
        let desired = Resource::with_provider("awscc", "service.Widget", "main", None);

        let first = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            std::slice::from_ref(&desired),
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .unwrap();
        assert_eq!(
            first,
            DeposedRefreshSummary {
                total_generations: 1,
                removed_generations: 0,
                updated_generations: 1,
                failed_generations: 0,
            }
        );
        let after_first = state
            .find_resource("awscc", "service.Widget", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .clone();
        assert!(
            after_first["status"].get("Enum").is_some(),
            "enum state should be stored in canonical enum shape"
        );

        let second = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            std::slice::from_ref(&desired),
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .unwrap();
        let after_second = state
            .find_resource("awscc", "service.Widget", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .clone();

        assert_eq!(
            second,
            DeposedRefreshSummary {
                total_generations: 1,
                removed_generations: 0,
                updated_generations: 0,
                failed_generations: 0,
            }
        );
        assert_eq!(after_second, after_first);
    }

    #[tokio::test]
    async fn refresh_deposed_generation_matches_masking_authority_by_provider_instance() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("west-key"),
            "db-old",
            Some("west"),
            HashMap::from([("password".to_string(), json!("old-plain"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat(
            "awscc",
            "db.Instance",
            "main",
            Some("west".to_string()),
        );
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "db-old",
            State::existing(
                id.clone(),
                HashMap::from([("password".to_string(), string_value("plain-secret"))]),
            )
            .with_identifier("db-old"),
        );
        let wrong_instance =
            Resource::with_provider("awscc", "db.Instance", "main", Some("east".to_string()))
                .with_attribute(
                    "password",
                    Value::Deferred(DeferredValue::Secret(Box::new(string_value(
                        "plain-secret",
                    )))),
                );
        let right_instance =
            Resource::with_provider("awscc", "db.Instance", "main", Some("west".to_string()));

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[wrong_instance, right_instance],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let stored = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .attributes
            .get("password")
            .and_then(|value| value.as_str())
            .expect("password should remain plaintext because west desired is not secret");
        assert_eq!(stored, "plain-secret");
    }

    #[tokio::test]
    async fn refresh_deposed_generation_read_error_leaves_entry_untouched() {
        let key = deposed_key("error-key");
        let mut state = StateFile::new();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.deposed.push(deposed_instance(
            key.clone(),
            "vpc-old",
            None,
            HashMap::from([("vpc_id".to_string(), json!("vpc-old"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_name_compat("awscc", "ec2.Vpc", "main", None);
        let provider =
            DeposedRefreshTestProvider::default().with_read_error(&id, "vpc-old", "read failed");

        let summary = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let row = state.find_resource("awscc", "ec2.Vpc", "main").unwrap();
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].key, key);
        assert_eq!(
            row.deposed[0].attributes.get("vpc_id"),
            Some(&json!("vpc-old"))
        );
        assert_eq!(
            summary,
            DeposedRefreshSummary {
                total_generations: 1,
                removed_generations: 0,
                updated_generations: 0,
                failed_generations: 1,
            }
        );
    }

    #[test]
    fn current_instance_refresh_preserves_deposed_entries() {
        let key = deposed_key("old-key");
        let id = ResourceId::with_provider_name_compat("awscc", "ec2.Vpc", "main", None);
        let mut state = StateFile::new();
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-current")
            .with_attribute("vpc_id", json!("vpc-current"));
        row.deposed.push(deposed_instance(
            key.clone(),
            "vpc-old",
            None,
            HashMap::from([("vpc_id".to_string(), json!("vpc-old"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");
        let desired = Resource::with_provider("awscc", "ec2.Vpc", "main", None);
        let fresh = State::existing(
            id.clone(),
            HashMap::from([("vpc_id".to_string(), string_value("vpc-current"))]),
        )
        .with_identifier("vpc-current");
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &id,
            &fresh,
            &mut state,
            Some(&desired),
            &SchemaRegistry::new(),
            "",
            &mut counts,
        )
        .unwrap();

        let row = state.find_resource("awscc", "ec2.Vpc", "main").unwrap();
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].key, key);
        assert_eq!(row.deposed[0].identifier, "vpc-old");
    }

    // --- run_force_unlock tests ---

    #[tokio::test]
    async fn force_unlock_without_backend_uses_local_backend() {
        // When no backend block is configured, run_force_unlock should
        // fall back to the anchored default local backend instead of erroring with
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
                // Expected: local backend found no lock file for the dummy ID
            }
            Err(AppError::Config(msg)) if msg.contains("No backend configuration found") => {
                panic!(
                    "force-unlock should fall back to local backend, got: {}",
                    msg
                );
            }
            Ok(()) => {
                panic!("expected LockNotFound error for dummy lock ID, got Ok");
            }
            other => {
                panic!("unexpected error: {:?}", other);
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

        let vpc_resource = plan.effects()[0].as_resource_ref().unwrap();
        assert!(vpc_resource.attributes().contains_key("cidr_block"));
        assert!(vpc_resource.attributes().contains_key("vpc_id"));
    }

    #[test]
    fn build_plan_from_state_empty() {
        let state = StateFile::new();
        let plan = build_plan_from_state(&state);
        assert!(plan.effects().is_empty());
    }

    #[test]
    fn build_plan_from_state_preserves_dependency_bindings() {
        let state = load_fixture_state();
        let plan = build_plan_from_state(&state);

        // subnet depends on vpc
        let subnet_resource = plan.effects()[1].as_resource_ref().unwrap();
        assert_eq!(
            subnet_resource.dependency_bindings(),
            &std::collections::BTreeSet::from(["vpc".to_string()])
        );
    }

    #[test]
    fn deferred_data_source_state_refresh_warning_names_read_and_missing_upstream() {
        let resource = DataSource::with_provider("mock", "iam.Roles", "roles", None);
        let warning = format_deferred_state_refresh_warning(
            &resource,
            &[UnresolvedDataSourceInput {
                attribute: "name_regex".to_string(),
                paths: vec![carina_core::resource::AccessPath::new(
                    "target_role",
                    "role_name",
                )],
                bindings: Vec::new(),
                unknowns: Vec::new(),
            }],
        );

        assert!(warning.contains("mock.iam.Roles.roles"));
        assert!(warning.contains("target_role.role_name"));
        assert!(warning.contains("skipped refreshing data source"));
    }

    #[test]
    fn deferred_data_source_state_refresh_warning_names_recorded_dependency() {
        let mut resource = DataSource::with_provider("mock", "iam.Roles", "roles", None);
        resource
            .dependency_bindings
            .insert("registry_publish.target".to_string());

        let warning = format_deferred_state_refresh_warning(&resource, &[]);

        assert!(
            warning.contains("registry_publish.target"),
            "warning must name the recorded dependency that caused deferral: {warning}"
        );
        assert!(!warning.trim_end().ends_with(':'));
    }

    // --- carina#3338: module-prefixed bindings + exports.<key> ---

    #[test]
    fn lookup_module_prefixed_binding_full_resource() {
        // `let r = usecase { … }` produces resources whose binding is
        // stored as `r.<inner>` in state. `carina state list` already
        // prints `r.distribution` as the display name — `state lookup`
        // must accept the same address.
        let state = load_fixture("state_lookup_modules_exports");
        let output = format_state_lookup(&state, "r.distribution", false).unwrap();
        assert!(
            output.contains("E2E954VKWYKT8K"),
            "expected full-resource lookup of r.distribution to include the id; got: {}",
            output
        );
    }

    #[test]
    fn lookup_module_prefixed_binding_attribute() {
        // `r.distribution.id` must resolve to the `id` attribute on
        // the resource whose binding is `r.distribution` — the actual
        // command users want to script against.
        let state = load_fixture("state_lookup_modules_exports");
        let output = format_state_lookup(&state, "r.distribution.id", false).unwrap();
        assert_eq!(output, "E2E954VKWYKT8K");
    }

    #[test]
    fn lookup_module_prefixed_binding_attribute_json() {
        let state = load_fixture("state_lookup_modules_exports");
        let output = format_state_lookup(&state, "r.distribution.id", true).unwrap();
        assert_eq!(output, "\"E2E954VKWYKT8K\"");
    }

    #[test]
    fn lookup_mistyped_module_prefixed_address_names_full_query() {
        // Regression pin: when neither rule (1) nor (2) matches, the
        // error must name the full query — not just the head before
        // the first dot. A user who typed `r.bogus.id` should see
        // their typo in the message, not the unhelpful `r`.
        let state = load_fixture("state_lookup_modules_exports");
        let err = format_state_lookup(&state, "r.bogus.id", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'r.bogus.id'"),
            "expected error to quote the full query; got: {}",
            msg
        );
    }

    #[test]
    fn lookup_module_prefixed_outer_alone_errors() {
        // `r` by itself is not a resource — only `r.<inner>` is. The
        // error message should reflect the actual unresolved address.
        let state = load_fixture("state_lookup_modules_exports");
        let err = format_state_lookup(&state, "r", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'r'"),
            "expected error mentioning 'r' was unresolved; got: {}",
            msg
        );
    }

    #[test]
    fn find_resource_by_module_prefixed_binding() {
        let state = load_fixture("state_lookup_modules_exports");
        let found = find_resource_by_query(&state, "r.distribution").unwrap();
        assert_eq!(found.resource_type, "cloudfront.Distribution");
    }

    #[test]
    fn lookup_exports_scalar() {
        // `exports.<key>` reads from state.exports, the deliberate
        // downstream contract operators script against from CI / shell.
        let state = load_fixture("state_lookup_modules_exports");
        let output =
            format_state_lookup(&state, "exports.cloudfront_distribution_id", false).unwrap();
        assert_eq!(output, "E2E954VKWYKT8K");
    }

    #[test]
    fn lookup_exports_scalar_json() {
        let state = load_fixture("state_lookup_modules_exports");
        let output =
            format_state_lookup(&state, "exports.cloudfront_distribution_id", true).unwrap();
        assert_eq!(output, "\"E2E954VKWYKT8K\"");
    }

    #[test]
    fn lookup_exports_list() {
        // List/object exports should round-trip as pretty JSON in both
        // modes (raw and --json), matching how resource-attribute
        // composites already render.
        let state = load_fixture("state_lookup_modules_exports");
        let output = format_state_lookup(&state, "exports.nameservers", false).unwrap();
        assert!(output.contains("ns-1234.awsdns-12.com"));
        assert!(output.contains("ns-5678.awsdns-56.net"));
    }

    #[test]
    fn lookup_exports_full_emits_object() {
        // `exports` with no key returns the full exports map as JSON.
        // Symmetrical with `lookup <binding>` returning the full
        // attributes map.
        let state = load_fixture("state_lookup_modules_exports");
        let output = format_state_lookup(&state, "exports", false).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["cloudfront_distribution_id"], "E2E954VKWYKT8K");
        assert_eq!(parsed["zone_id"], "Z008131930MO3U3NYWJTM");
    }

    #[test]
    fn lookup_exports_missing_key_errors() {
        let state = load_fixture("state_lookup_modules_exports");
        let err = format_state_lookup(&state, "exports.does_not_exist", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'does_not_exist'") && msg.to_lowercase().contains("export"),
            "expected error to mention the missing export key; got: {}",
            msg
        );
    }

    #[test]
    fn lookup_exports_resource_binding_named_exports_takes_precedence() {
        // Edge case: if a user happens to bind a resource as `exports`,
        // the resource lookup must still win — they've named it
        // explicitly, and changing the meaning would silently shadow
        // their resource. Use the export-key form `exports.<key>` only
        // when no resource by that name exists.
        //
        // This pins the precedence so a future refactor can't flip it.
        let mut state = StateFile::new();
        let mut rs = ResourceState::new("ec2.Vpc", "exports-vpc", "awscc");
        rs.binding = Some("exports".to_string());
        rs.attributes
            .insert("vpc_id".to_string(), serde_json::json!("vpc-from-resource"));
        state
            .upsert_resource(rs)
            .expect("test state setup must be valid");
        state
            .exports
            .insert("vpc_id".to_string(), serde_json::json!("from-export"));

        // `exports.vpc_id` should find the resource's attribute, not
        // the export key, because a `binding = "exports"` resource
        // exists.
        let output = format_state_lookup(&state, "exports.vpc_id", false).unwrap();
        assert_eq!(output, "vpc-from-resource");
    }

    #[test]
    fn completion_module_prefixed_bindings() {
        // Tab-completion must offer module-prefixed bindings as
        // candidates, otherwise the operator has no way to discover
        // them short of reading the JSON.
        let state = load_fixture("state_lookup_modules_exports");
        let candidates = complete_state_lookup_from(&state, "r.");
        let values = candidate_values(&candidates);
        assert!(
            values.contains(&"r.bucket".to_string()),
            "expected r.bucket among completions; got: {:?}",
            values
        );
        assert!(
            values.contains(&"r.distribution".to_string()),
            "expected r.distribution among completions; got: {:?}",
            values
        );
        assert!(
            values.contains(&"r.zone".to_string()),
            "expected r.zone among completions; got: {:?}",
            values
        );
    }

    #[test]
    fn completion_exports_keys_after_dot() {
        // `exports.` should complete to the keys in state.exports.
        let state = load_fixture("state_lookup_modules_exports");
        let candidates = complete_state_lookup_from(&state, "exports.");
        let values = candidate_values(&candidates);
        assert!(
            values.contains(&"exports.cloudfront_distribution_id".to_string()),
            "expected exports.cloudfront_distribution_id among completions; got: {:?}",
            values
        );
        assert!(
            values.contains(&"exports.zone_id".to_string()),
            "expected exports.zone_id among completions; got: {:?}",
            values
        );
    }

    #[test]
    fn completion_attribute_on_module_prefixed_binding() {
        // After typing `r.distribution.` the completer must resolve
        // the module-prefixed binding and offer that resource's
        // attribute keys, not collapse to top-level bindings.
        // Pins the longest-prefix resolver wiring in the completion
        // path (distinct logic from `format_state_lookup`).
        let state = load_fixture("state_lookup_modules_exports");
        let candidates = complete_state_lookup_from(&state, "r.distribution.");
        let values = candidate_values(&candidates);
        assert!(
            values.contains(&"r.distribution.id".to_string()),
            "expected r.distribution.id among completions; got: {:?}",
            values
        );
        assert!(
            values.contains(&"r.distribution.domain_name".to_string()),
            "expected r.distribution.domain_name among completions; got: {:?}",
            values
        );
    }

    #[test]
    fn completion_exports_top_level() {
        // Empty / `e` prefix should surface `exports` itself as a
        // candidate (so it's discoverable without docs), but only when
        // the state actually has exports.
        let state = load_fixture("state_lookup_modules_exports");
        let candidates = complete_state_lookup_from(&state, "e");
        let values = candidate_values(&candidates);
        assert!(
            values.contains(&"exports".to_string()),
            "expected `exports` candidate for partial `e`; got: {:?}",
            values
        );
    }
}
