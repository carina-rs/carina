use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use colored::Colorize;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::effect::Effect;
use carina_core::parser::ProviderContext;
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer};
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};
use carina_core::value::{format_value, json_to_dsl_value};
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, LockInfo, ResourceState, StateBackend,
    StateFile, StateUrl, create_backend, load_state_from_url, resolve_backend_anchored,
    resolve_backend_for_read,
};

use super::{
    BackendDriftStatus, DriftCommand, inspect_backend_drift, validate_and_resolve_with_config,
    verify_for_mutation,
};
use crate::commands::shared::state_writeback::apply_name_overrides;
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, get_provider_with_ctx,
    read_data_source_with_retry, reconcile_anonymous_identifiers_with_ctx,
    reconcile_prefixed_names, resolve_data_source_refs_for_refresh,
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
        .resources
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
    for rs in &state.resources {
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
    cancel: CancellationToken,
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
    let backend: Box<dyn StateBackend> =
        resolve_backend_anchored(backend_config.as_ref(), base_dir)
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

    let backend: Box<dyn StateBackend> = resolve_backend_for_read(parsed.backend.as_ref())
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
        .resources
        .iter()
        .find(|r| r.binding.as_deref() == Some(name))
        .or_else(|| {
            // Fall back to identity
            state.resources.iter().find(|r| r.identity == name)
        })
}

/// Format state list output. Returns each line as a string.
fn format_state_list(state: &StateFile) -> Vec<String> {
    state
        .resources
        .iter()
        .map(|rs| {
            let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
            format!("{}.{} {}", rs.provider, rs.resource_type, display_name)
        })
        .collect()
}

/// Run state list command
async fn run_state_list(
    path: Option<&Path>,
    state_url: Option<&str>,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = load_state_file(path, state_url, provider_context).await?;

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
    for rs in &state.resources {
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
        Some(attr) => {
            let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
            let value = rs.attributes.get(attr).ok_or_else(|| {
                AppError::Config(format!(
                    "Attribute '{}' not found on resource '{}'.",
                    attr, display_name
                ))
            })?;
            if json_output {
                Ok(serde_json::to_string_pretty(value).unwrap())
            } else {
                Ok(format_raw_value(value))
            }
        }
        None => {
            let sorted: std::collections::BTreeMap<_, _> = rs.attributes.iter().collect();
            Ok(serde_json::to_string_pretty(&sorted).unwrap())
        }
    }
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
    for rs in &state.resources {
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

        plan.add(Effect::Read { resource });
    }
    plan
}

/// Format state show output (non-TUI mode).
///
/// Shows all resources with their type, identity/binding, and full attributes.
fn format_state_show(state: &StateFile) -> String {
    let mut output = String::new();
    for (i, rs) in state.resources.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        let display_name = rs.binding.as_deref().unwrap_or(&rs.identity);
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

    if state.resources.is_empty() {
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
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir);
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
    cancel: CancellationToken,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve_with_config(&mut parsed, base_dir, true)?;

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
        cancel,
    )
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = backend.release_lock(li).await.map_err(AppError::Backend);

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
    cancel: CancellationToken,
) -> Result<(), AppError> {
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);

    // Read current state from backend. carina#3315: persist any older-schema
    // migration under the refresh lock before the "no
    // resources" short-circuit returns — see
    // `apply::load_state_persist_if_migrated`. The on-disk version
    // must advance so the carina#3283 warning text matches reality.
    let mut state_file =
        crate::commands::apply::load_state_persist_if_migrated(backend, lock).await?;

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
        );
    }
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
    if cancel.is_cancelled() {
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
    let saved_attrs_for_expansion = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
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
    if cancel.is_cancelled() {
        return Err(AppError::Interrupted);
    }

    // Also read states for orphaned resources (in state but removed from config)
    let desired_ids: HashSet<ResourceId> = sorted_resources.iter().map(|r| r.id.clone()).collect();
    let orphan_ids: Vec<(ResourceId, String)> = state_file
        .as_ref()
        .map(|sf| {
            sf.resources
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
        let resolved_data_sources = resolve_data_source_refs_for_refresh(
            &sorted_resources,
            &parsed.compositions,
            &parsed.data_sources,
            &current_states,
            &HashMap::new(),
            ctx.schemas(),
            &wait_aliases,
        )?;
        for resource in &resolved_data_sources {
            if cancel.is_cancelled() {
                return Err(AppError::Interrupted);
            }
            let fresh_state = read_data_source_with_retry(&provider, resource)
                .await
                .map_err(AppError::Provider)?;
            if cancel.is_cancelled() {
                return Err(AppError::Interrupted);
            }
            current_states.insert(resource.id.clone(), fresh_state);
        }
    }

    // Restore unreturned attributes from state file (CloudControl doesn't always return them)
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    // awscc#251: lift pre-Enum-migration state before
    // `hydrate_read_state` carries it forward into read state.
    // carina#3272: use the post-expansion `sorted_resources` (not
    // `parsed.resources`) so for-loop-materialised children's saved
    // state is lifted too — otherwise their enum-typed attrs stay
    // as plain `String` and the differ surfaces a phantom case diff.
    carina_core::utils::lift_saved_state_enum_leaves(
        &mut saved_attrs,
        &sorted_resources,
        ctx.schemas(),
    );
    provider
        .hydrate_read_state(&mut current_states, &saved_attrs)
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

    // Re-resolve exports using refreshed state
    if !parsed.export_params.is_empty() {
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
            &wait_aliases,
        )?;
        resolution.write_into(&mut state);
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

async fn refresh_existing_resources_until_cancelled(
    provider: &dyn Provider,
    reads: Vec<(ResourceId, String)>,
    cancel: &CancellationToken,
) -> Result<(HashMap<ResourceId, State>, HashSet<ResourceId>), AppError> {
    let mut current_states = HashMap::new();
    let mut refreshed = HashSet::new();
    let mut read_iter = reads.into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut refresh_cancelled = cancel.is_cancelled();

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
            in_flight.next().await.unwrap()
        } else {
            tokio::select! {
                biased;
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

/// Compare old state with fresh provider state for a single resource,
/// display any changes, and update the state file accordingly.
///
/// When `resource` is `Some`, directives, prefixes, and desired keys
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
    let existing = state.find_resource(&id.provider, &id.resource_type, id.identity_or_empty());
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
            id.identity_or_empty(),
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
                owned_resource = Resource::with_provider(
                    &id.provider,
                    &id.resource_type,
                    id.identity_or_empty(),
                    id.provider_instance.clone(),
                );
                &owned_resource
            }
        };
        let existing_rs =
            state.find_resource(&id.provider, &id.resource_type, id.identity_or_empty());
        let resource_state = ResourceState::from_provider_state(res, fresh_state, existing_rs)?;
        state.upsert_resource(resource_state);
    } else {
        state.remove_resource(&id.provider, &id.resource_type, id.identity_or_empty());
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
        serde_json::from_str(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path.display(), e))
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
        state.upsert_resource(rs1);
        state.upsert_resource(rs2);

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
        state.upsert_resource(rs);
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
