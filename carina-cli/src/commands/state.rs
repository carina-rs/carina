use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use colored::Colorize;
use futures::stream::{FuturesUnordered, StreamExt};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::data_source_value::DataSourceValue;
#[cfg(test)]
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{Orphan, Orphans, into_all_prior_states, lift_prior_state_enum_leaves};
use carina_core::effect::Effect;
use carina_core::executor::UnresolvedDataSourceInput;
use carina_core::parser::{ProviderContext, ResourceTypePath, StateBlockAddress};
use carina_core::plan::Plan;
use carina_core::plan::{
    CreateOutcomeUnknownAbsentResolution, create_outcome_unknown_absent_resolution,
};
use carina_core::provider::{
    self as provider_mod, AppliedIdentifier, InFlightCreate, PersistedAttributes, Provider,
    ProviderError, ProviderNormalizer, ProviderRouter, RawSavedAttrs, UnknownCreateOutcome,
};
use carina_core::resource::{
    Current, DataSource, ResolvedDataSource, ResolvedResource, ResolvedResourceId, ResourceId,
    ResourceIdentity, State, Value,
};
use carina_core::shutdown::{
    CleanupInterrupted, LoopShutdownPhase, LoopStep, ShutdownPhase, ShutdownToken,
};
use carina_core::value::format_value;
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, BackendSelection, DeposedInstance,
    DeposedKey, LoadedOrphans, LockInfo, ResourceState, RowBasis, StateBackend, StateFile,
    StateUrl, load_state_from_url,
};

use super::{
    BackendLockRequirement, DriftCommand, ReadBackendWarningPolicy, resolve_backend_for_read,
    verify_for_mutation,
};
use crate::commands::plan::{
    ProviderUpstreamScope, ResolvedProviderInputs, UpstreamMissingStatePolicy,
    load_and_resolve_provider_configs,
};
use crate::commands::shared::finalize::release_lock_after_execute;
use crate::commands::shared::lock_heartbeat::{StateLock, StateLockRef};
use crate::commands::shared::progress::{
    ProgressBackend, ProgressSink, RefreshPhase, emit_newline_on_interrupt,
};
use crate::commands::shared::state_writeback::SkippedExports;
use crate::commands::state_output::{StateShowCurrentKind, StateShowDocument, StateShowResource};
use crate::cursor::CursorReveal;
use crate::error::AppError;
use crate::wiring::{
    AddressedDataSourceReadInputs, ProviderLoad, ValidatedConfig, get_provider_with_ctx,
    prepare_default_provider_ready_config, refresh_loaded_orphan_with_retry,
};
use carina_core::hint::ProjectCommand;

/// Convert a lock acquisition error into an `AppError`.
///
/// For `Locked` errors, includes a hint about `force-unlock`.
/// All other backend errors are passed through as `AppError::Backend`.
pub fn map_lock_error(e: BackendError, project_dir: &Path) -> AppError {
    match e {
        BackendError::Locked {
            who,
            lock_id,
            operation,
        } => AppError::Config(format!(
            "State is locked by {} (lock ID: {}, operation: {})\n\
             If you believe this is stale, run: `{}`",
            who,
            lock_id,
            operation,
            ProjectCommand::new("force-unlock", project_dir).with_argument(lock_id.as_str())
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
/// Delegates to [`complete_state_lookup_from`], which completes resource
/// bindings/names (including module-prefixed bindings such as
/// `r.distribution`) and their attributes.
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
/// Two candidate spaces are produced (carina#3338):
///
/// - **Resource bindings / identities**: surface every binding the state
///   carries — module-prefixed (`r.distribution`) shows up as one
///   candidate, just like `state list` already prints it. No splitting
///   on `.`; matched by `starts_with(current)`. (Top-level identities
///   with no binding fall back to `rs.identity`.)
/// - **Resource attributes**: once a resource address and `.` are present,
///   surface the persisted attribute names on that resource.
fn complete_state_lookup_from(state: &StateFile, current: &str) -> Vec<CompletionCandidate> {
    // Attribute completion for a known resource: `<binding>.<attr>`.
    // Use the address resolver so module-prefixed bindings ride the
    // same longest-prefix rule as `format_state_lookup`.
    if let Some((before_dot, _)) = current.rsplit_once('.')
        && let Some((rs, _)) = resolve_resource_address(state, before_dot)
    {
        let persisted = rs.persisted_attributes();
        return persisted
            .into_iter()
            .flat_map(PersistedAttributes::keys)
            .filter(|key| {
                let full = format!("{}.{}", before_dot, key);
                full.starts_with(current)
            })
            .map(|key| CompletionCandidate::new(format!("{}.{}", before_dot, key)))
            .collect();
    }

    // Top-level resource bindings and identities.
    let mut candidates: Vec<CompletionCandidate> = Vec::new();
    for rs in state.resources() {
        let display_name = rs.binding.as_deref().unwrap_or(rs.identity.as_str());
        if display_name.starts_with(current) {
            candidates.push(CompletionCandidate::new(display_name));
        }
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
    /// Forget a create-outcome-unknown row after confirming the resource is absent
    Forget {
        /// Namespaced resource type, for example awscc.ec2.Vpc
        resource_type: String,

        /// Resource identity
        identity: String,

        /// Skip confirmation prompt (auto-approve)
        #[arg(long)]
        auto_approve: bool,

        /// Path to directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
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

impl StateCommands {
    pub fn project_dir(&self) -> Option<&Path> {
        match self {
            Self::BucketDelete { path, .. }
            | Self::Refresh { path, .. }
            | Self::Forget { path, .. } => Some(path),
            Self::List { path, state_url }
            | Self::Lookup {
                path, state_url, ..
            }
            | Self::Show {
                path, state_url, ..
            } => {
                if state_url.is_some() {
                    None
                } else {
                    Some(path.as_deref().unwrap_or_else(|| Path::new(".")))
                }
            }
        }
    }
}

/// Run state subcommands
pub async fn run_state_command(
    user_dirs: &carina_core::UserDirs,
    command: StateCommands,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    match command {
        StateCommands::BucketDelete {
            bucket_name,
            force,
            path,
        } => run_state_bucket_delete(user_dirs, &bucket_name, force, &path, provider_context).await,
        StateCommands::Refresh { path, lock } => {
            run_state_refresh(user_dirs, &path, lock, provider_context, cancel).await
        }
        StateCommands::Forget {
            resource_type,
            identity,
            auto_approve,
            path,
        } => {
            run_state_forget(
                &resource_type,
                &identity,
                auto_approve,
                &path,
                provider_context,
                cancel,
            )
            .await
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateForgetConfirmation {
    Confirmed,
    Cancelled,
}

fn state_forget_address(
    namespaced_type: &str,
    identity: &str,
) -> Result<StateBlockAddress, AppError> {
    let type_path = ResourceTypePath::parse(namespaced_type).ok_or_else(|| {
        AppError::Config(format!(
            "Invalid resource type '{namespaced_type}'; expected a namespaced type such as awscc.ec2.Vpc"
        ))
    })?;
    let identity = ResourceIdentity::try_from(identity.to_owned())?;
    Ok(StateBlockAddress::new(
        type_path.provider,
        type_path.resource_type,
        identity,
    ))
}

fn display_state_forget_address(address: &StateBlockAddress) -> String {
    format!(
        "{} {}",
        address.display_type(),
        carina_core::parser::display_string_literal(address.name_str())
    )
}

fn format_state_forget_target(
    address: &StateBlockAddress,
    row: &carina_state::ResourceState,
) -> String {
    let mut lines = vec![
        format!(
            "Create-outcome-unknown row: {}",
            display_state_forget_address(address)
        ),
        "This row has no provider identifier and cannot be destroyed safely.".to_string(),
    ];
    if !row.deposed.is_empty() {
        lines.push(
            "Deposed generations that will remain tracked for deletion by the next plan:"
                .to_string(),
        );
        lines.extend(row.deposed.iter().map(|deposed| {
            format!(
                "  - identifier `{}` (generation `{}`, provider instance `{}`)",
                deposed.identifier,
                deposed.key,
                deposed.provider_instance.as_deref().unwrap_or("default")
            )
        }));
    }
    lines.join("\n")
}

async fn confirm_state_forget<R>(
    reader: R,
    cancel: ShutdownToken,
    auto_approve: bool,
) -> Result<StateForgetConfirmation, AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    if auto_approve {
        return Ok(StateForgetConfirmation::Confirmed);
    }

    println!(
        "{}",
        "Forget this create-outcome-unknown state row?"
            .yellow()
            .bold()
    );
    println!(
        "  {}",
        "Only continue after confirming that the resource does not exist. Type 'yes' to confirm."
            .yellow()
    );
    let input = {
        let _reveal = CursorReveal::new();
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|error| error.to_string())?;
        let result = crate::signal::read_line_until_cancelled(reader, cancel).await;
        emit_newline_on_interrupt(&mut std::io::stdout(), &result);
        result?
    };

    if input.trim() == "yes" {
        println!();
        Ok(StateForgetConfirmation::Confirmed)
    } else {
        println!();
        println!("{}", "State forget cancelled.".yellow());
        Ok(StateForgetConfirmation::Cancelled)
    }
}

/// Remove the recovery row for a create whose outcome is unknown.
pub async fn run_state_forget(
    namespaced_type: &str,
    identity: &str,
    auto_approve: bool,
    path: &Path,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    let address = state_forget_address(namespaced_type, identity)?;
    let parsed = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?
    .parsed;
    let verified_backend =
        verify_for_mutation(path, parsed.backend.as_ref(), DriftCommand::ForgetState)?;
    let backend = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?;

    println!("{}", "Acquiring state lock...".cyan());
    let lock = backend
        .acquire_lock("state forget")
        .await
        .map_err(|error| map_lock_error(error, path))?;
    println!("  {} Lock acquired", "✓".green());

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let operation = run_state_forget_locked(
        backend.as_ref(),
        &lock,
        &address,
        path,
        auto_approve,
        reader,
        cancel.clone(),
    )
    .await;
    let release =
        release_lock_after_execute(backend.as_ref(), StateLockRef::Held(&lock), &cancel).await;
    if release.is_ok() {
        println!("  {} Lock released", "✓".green());
    }
    operation?;
    release
}

async fn run_state_forget_locked<R>(
    backend: &dyn StateBackend,
    lock: &LockInfo,
    address: &StateBlockAddress,
    project_dir: &Path,
    auto_approve: bool,
    reader: R,
    cancel: ShutdownToken,
) -> Result<(), AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut state =
        crate::commands::apply::load_state_persist_if_migrated(backend, StateLockRef::Held(lock))
            .await?
            .unwrap_or_default();
    let row = state
        .find_outcome_unknown(address)
        .map_err(|source| AppError::from_state_forget(source, project_dir))?;

    println!("{}", format_state_forget_target(address, row.resource()));
    if confirm_state_forget(reader, cancel, auto_approve).await?
        == StateForgetConfirmation::Cancelled
    {
        return Ok(());
    }

    match create_outcome_unknown_absent_resolution() {
        CreateOutcomeUnknownAbsentResolution::StateForget => {
            StateFile::forget(row);
        }
    }
    crate::commands::apply::save_state_locked(backend, lock, &mut state).await?;
    println!(
        "{} {}",
        "Forgot create-outcome-unknown current instance:"
            .green()
            .bold(),
        display_state_forget_address(address)
    );
    Ok(())
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

    // Administrative lock recovery uses the shared old-backend-on-drift
    // selector without emitting the read-command warning.
    println!("{}", "Force unlocking state...".yellow().bold());
    println!("Lock ID: {}", lock_id);

    match BackendSelection::force_unlock(base_dir, parsed.backend.as_ref(), lock_id).await {
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
) -> Result<Option<StateFile>, AppError> {
    if let Some(raw) = state_url {
        let url = StateUrl::parse(raw).map_err(AppError::Backend)?;
        return load_state_from_url(&url)
            .await
            .map(Some)
            .map_err(AppError::Backend);
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
    let backend = resolve_backend_for_read(
        base_dir,
        path,
        parsed.backend.as_ref(),
        BackendLockRequirement::AllowMissing,
        ReadBackendWarningPolicy::CommandBoundary { project_dir: path },
    )
    .await?;

    backend
        .read_state()
        .await
        .map(|state| state.map(|loaded| loaded.into_state()))
        .map_err(AppError::Backend)
}

fn require_state_file(state: Option<StateFile>) -> Result<StateFile, AppError> {
    state.ok_or_else(|| AppError::Config("No state file found.".to_string()))
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
            state
                .resources()
                .iter()
                .find(|r| r.identity.as_str() == name)
        })
}

#[derive(Clone, Copy)]
enum StateCommandStateKind {
    Absent,
    Present,
    MissingIdentifier,
}

/// State-command presentation boundary for a managed current instance.
///
/// Keeping the exhaustive [`Current`] match here prevents state subcommands
/// from flattening an in-flight create into an empty attribute-bearing state.
enum StateCommandCurrent<'a> {
    State {
        state: &'a State,
        kind: StateCommandStateKind,
    },
    CreateInFlight(&'a InFlightCreate),
    CreateOutcomeUnknown(&'a UnknownCreateOutcome),
}

impl<'a> From<&'a Current> for StateCommandCurrent<'a> {
    fn from(current: &'a Current) -> Self {
        match current {
            Current::Absent(absent) => Self::State {
                state: absent.state(),
                kind: StateCommandStateKind::Absent,
            },
            Current::Present(present) => Self::State {
                state: present.state(),
                kind: StateCommandStateKind::Present,
            },
            Current::CreateInFlight(pending) => Self::CreateInFlight(pending),
            Current::CreateOutcomeUnknown(unknown) => Self::CreateOutcomeUnknown(unknown),
            Current::MissingIdentifier(unidentified) => Self::State {
                state: unidentified.state(),
                kind: StateCommandStateKind::MissingIdentifier,
            },
        }
    }
}

fn current_for_state_command(state: &StateFile, row: &ResourceState) -> Current {
    let id = ResourceId::with_provider_identity(
        row.provider.clone(),
        row.resource_type.clone(),
        row.identity.clone(),
        row.directives.provider_instance.clone(),
    );
    state.build_state_for_resource(&id)
}

fn create_in_flight_status(pending: &InFlightCreate) -> String {
    format!("create in flight since {}", pending.started_at().as_str())
}

fn create_outcome_unknown_status(unknown: &UnknownCreateOutcome) -> String {
    format!(
        "create outcome unknown since {}; the create may have reached the cloud",
        unknown.started_at().as_str()
    )
}

/// Format state list output. Returns each line as a string.
fn format_state_list(state: &StateFile) -> Vec<String> {
    let mut lines = Vec::new();
    for rs in state.resources() {
        let display_name = rs.binding.as_deref().unwrap_or(rs.identity.as_str());
        let row_prefix = format!("{}.{} {}", rs.provider, rs.resource_type, display_name);
        let current = current_for_state_command(state, rs);
        match StateCommandCurrent::from(&current) {
            StateCommandCurrent::State {
                state: _state,
                kind,
            } => match kind {
                StateCommandStateKind::Absent => {
                    lines.push(format!("{row_prefix}  (no current instance)"));
                }
                StateCommandStateKind::Present => lines.push(row_prefix.clone()),
                StateCommandStateKind::MissingIdentifier => {
                    lines.push(format!("{row_prefix}  (missing provider identifier)"));
                }
            },
            StateCommandCurrent::CreateInFlight(pending) => {
                lines.push(format!(
                    "{row_prefix}  ({})",
                    create_in_flight_status(pending)
                ));
            }
            StateCommandCurrent::CreateOutcomeUnknown(unknown) => {
                lines.push(format!(
                    "{row_prefix}  ({})",
                    create_outcome_unknown_status(unknown)
                ));
            }
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
    let state = require_state_file(load_state_file(path, state_url, provider_context).await?)?;

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
/// Queries are resolved in this order:
///
/// 1. **Resource binding / name**, optionally followed by an attribute:
///    `vpc`, `vpc.vpc_id`, `r.distribution`, `r.distribution.id`. The
///    binding is matched by **longest-prefix**, so module-prefixed
///    bindings (`let r = usecase { … }` → resources stored as
///    `binding = "r.distribution"`) resolve the same way `state list`
///    already displays them (carina#3338). The longest-prefix scan
///    also subsumes the previous one-level form.
/// 2. When no resource address matches, the error names the full
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
        if json_output {
            return format_state_lookup_json(state, rs, attribute);
        }
        let current = current_for_state_command(state, rs);
        return format_resource_value(rs, &current, attribute, false);
    }

    // (2) Nothing matched — report the full query so the operator
    // sees the address they typed, not a stripped head.
    Err(AppError::Config(format!(
        "Resource '{}' not found in state.",
        query
    )))
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

fn format_state_lookup_json(
    state: &StateFile,
    row: &ResourceState,
    attribute: Option<&str>,
) -> Result<String, AppError> {
    let resource = StateShowResource::from_row(row)
        .map_err(|error| AppError::Config(format!("Failed to render state JSON: {error}")))?;

    let value = match attribute {
        None => serde_json::to_value(&resource)
            .expect("the public state resource type must always serialize"),
        Some(attribute) if resource.current.kind == StateShowCurrentKind::CreateInFlight => {
            let current = current_for_state_command(state, row);
            let StateCommandCurrent::CreateInFlight(pending) = StateCommandCurrent::from(&current)
            else {
                unreachable!("public and internal current kinds must agree")
            };
            let display_name = row.binding.as_deref().unwrap_or(row.identity.as_str());
            return Err(AppError::Config(format!(
                "Cannot look up attribute '{attribute}' on resource '{display_name}': {}.",
                create_in_flight_status(pending)
            )));
        }
        Some(attribute) => resource
            .current
            .attributes
            .get(attribute)
            .cloned()
            .ok_or_else(|| {
                let display_name = row.binding.as_deref().unwrap_or(row.identity.as_str());
                AppError::Config(format!(
                    "Attribute '{attribute}' not found on resource '{display_name}'."
                ))
            })?,
    };

    serde_json::to_string_pretty(&value)
        .map_err(|error| AppError::Config(format!("Failed to serialize state JSON: {error}")))
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
    current: &Current,
    attribute: Option<&str>,
    json_output: bool,
) -> Result<String, AppError> {
    match StateCommandCurrent::from(current) {
        StateCommandCurrent::State {
            state: _state,
            kind: _kind,
        } => match attribute {
            Some(attr) => format_resource_attribute_value(rs, attr, json_output),
            None if !rs.deposed.is_empty() => format_resource_full_value_with_deposed(rs),
            None => Ok(rs.format_attributes_for_state_show()),
        },
        StateCommandCurrent::CreateInFlight(pending) => match attribute {
            Some(attr) => {
                let display_name = rs.binding.as_deref().unwrap_or(rs.identity.as_str());
                Err(AppError::Config(format!(
                    "Cannot look up attribute '{attr}' on resource '{display_name}': {}.",
                    create_in_flight_status(pending)
                )))
            }
            None => {
                let status = create_in_flight_status(pending);
                if json_output {
                    Ok(serde_json::to_string_pretty(&status)
                        .expect("in-flight status is serializable"))
                } else {
                    Ok(status)
                }
            }
        },
        StateCommandCurrent::CreateOutcomeUnknown(unknown) => match attribute {
            Some(attr) => {
                let display_name = rs.binding.as_deref().unwrap_or(rs.identity.as_str());
                Err(AppError::Config(format!(
                    "Cannot look up attribute '{attr}' on resource '{display_name}': {}.",
                    create_outcome_unknown_status(unknown)
                )))
            }
            None => {
                let status = create_outcome_unknown_status(unknown);
                if json_output {
                    Ok(serde_json::to_string_pretty(&status)
                        .expect("outcome-unknown status is serializable"))
                } else {
                    Ok(status)
                }
            }
        },
    }
}

fn format_resource_attribute_value(
    rs: &ResourceState,
    attr: &str,
    json_output: bool,
) -> Result<String, AppError> {
    let Some(value) = rs.format_attribute_for_state_show(attr, json_output) else {
        return missing_attribute_error(rs, attr);
    };
    Ok(value)
}

fn format_resource_full_value_with_deposed(rs: &ResourceState) -> Result<String, AppError> {
    Ok(rs.format_generations_for_state_show())
}

fn missing_attribute_error<T>(rs: &ResourceState, attr: &str) -> Result<T, AppError> {
    let display_name = rs.binding.as_deref().unwrap_or(rs.identity.as_str());
    Err(AppError::Config(format!(
        "Attribute '{}' not found on resource '{}'.",
        attr, display_name
    )))
}

/// Run state lookup command
async fn run_state_lookup(
    query: &str,
    path: Option<&Path>,
    state_url: Option<&str>,
    json_output: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let state = require_state_file(load_state_file(path, state_url, provider_context).await?)?;
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
        let identity = rs.identity.clone();
        // carina#3181 PR D: `Effect::Read` carries a `DataSource`.
        let mut resource = carina_core::resource::DataSource::with_provider(
            &rs.provider,
            &rs.resource_type,
            identity,
            rs.directives.provider_instance.clone(),
        );
        resource.directives = rs.directives.clone();

        // Set typed metadata fields from state
        resource.binding = rs.binding.clone();
        resource.dependency_bindings = rs.dependency_bindings.clone();

        // Convert persisted JSON attributes through the state-only decoder.
        for (key, value) in rs
            .persisted_attributes()
            .map(PersistedAttributes::to_values)
            .unwrap_or_default()
        {
            resource.set_attr(key, value);
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
        let display_name = rs.binding.as_deref().unwrap_or(rs.identity.as_str());
        output.push_str(&format!(
            "# {}.{} ({})\n",
            rs.provider, rs.resource_type, display_name
        ));

        let current = current_for_state_command(state, rs);
        match StateCommandCurrent::from(&current) {
            StateCommandCurrent::State {
                state: _state,
                kind: _kind,
            } => format_attributes_for_show(
                &mut output,
                rs.persisted_attributes()
                    .map(PersistedAttributes::to_values)
                    .unwrap_or_default(),
                "  ",
            ),
            StateCommandCurrent::CreateInFlight(pending) => {
                output.push_str(&format!("  {}\n", create_in_flight_status(pending)));
            }
            StateCommandCurrent::CreateOutcomeUnknown(unknown) => {
                output.push_str(&format!("  {}\n", create_outcome_unknown_status(unknown)));
            }
        }
        for deposed in &rs.deposed {
            output.push_str(&format!(
                "  {}\n",
                deposed_state_marker(&deposed.key, &deposed.identifier)
            ));
            format_attributes_for_show(
                &mut output,
                deposed.persisted_attributes().to_values(),
                "    ",
            );
        }
    }
    output
}

fn format_attributes_for_show(
    output: &mut String,
    attributes: HashMap<String, Value>,
    indent: &str,
) {
    let mut keys: Vec<&String> = attributes.keys().collect();
    keys.sort();
    for key in keys {
        let value = &attributes[key];
        output.push_str(&format!("{}{} = {}\n", indent, key, format_value(value)));
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

    let Some(state) = state else {
        if json {
            let document = StateShowDocument::absent();
            let json = serde_json::to_string_pretty(&document)
                .map_err(|error| format!("Failed to serialize state JSON: {error}"))?;
            println!("{json}");
            return Ok(());
        }
        return Err(AppError::Config("No state file found.".to_string()));
    };

    if json {
        let document = StateShowDocument::from_state(&state)
            .map_err(|error| format!("Failed to render state JSON: {error}"))?;
        let json_str = serde_json::to_string_pretty(&document)
            .map_err(|error| format!("Failed to serialize state JSON: {error}"))?;
        println!("{}", json_str);
        return Ok(());
    }

    if state.resources().is_empty() {
        println!("No resources in state.");
        return Ok(());
    }

    if tui {
        let plan = build_plan_from_state(&state);
        let historical_display =
            carina_core::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
        let prev_explicit = state.build_explicit();
        carina_tui::run(
            &plan,
            &carina_core::schema::SchemaRegistry::new(),
            &historical_display,
            Some(&prev_explicit),
            &carina_core::provider::ProviderRouter::new(),
        )
        .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
    } else {
        let output = format_state_show(&state);
        print!("{}", output);
    }

    Ok(())
}

/// Run state bucket delete command
async fn run_state_bucket_delete(
    user_dirs: &carina_core::UserDirs,
    bucket_name: &str,
    force: bool,
    path: &Path,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let parsed = loaded.parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found.")?;
    let state_config = StateBackendConfig::try_from(backend_config)
        .map_err(carina_state::BackendError::from)
        .map_err(AppError::Backend)?;
    let settings = state_config
        .as_s3()
        .ok_or("The configured backend does not manage an S3 bucket")?;
    // Bucket deletion intentionally targets the configured NEW backend during
    // pending migration. This typed selection proves that the requested name
    // is that configured bucket; delete an orphaned OLD bucket out of band.
    let selection = BackendSelection::for_bucket_delete(settings, bucket_name)
        .map_err(|error| AppError::Config(error.into_message()))?;

    let base_dir = get_base_dir(path);
    // Provider loading depends only on the authored source/version. Validate
    // that command wiring before asking the operator to confirm deletion; the
    // resolved provider values needed for construction can wait until after
    // confirmation.
    let providers = ProviderLoad::load(user_dirs, &parsed.providers, base_dir)?;
    let validated_config = providers.validate_evaluated_config(
        parsed,
        base_dir,
        true,
        &inference_errors,
        &duplicate_declarations,
    )?;

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

    let provider_name = selection.provider_name();
    let resource_type = selection.resource_type();
    let provider_inputs = load_state_bucket_delete_provider_inputs(
        validated_config.resources(),
        base_dir,
        provider_context,
    )
    .await?;
    let bucket_provider = create_state_bucket_delete_provider(
        &validated_config,
        provider_inputs.configs(),
        provider_name,
    )
    .await?;

    // First, try to empty the bucket (delete all objects and versions)
    println!();
    println!("{}", "Emptying bucket...".cyan());

    // Delete the bucket resource (identifier is the bucket name)
    // Backend bucket is provider-default; named-instance routing is
    // a DSL concern that doesn't apply to the implicit state bucket.
    let bucket_identity = ResourceIdentity::try_from(bucket_name.to_string()).map_err(|error| {
        AppError::InvalidStateBucketIdentity {
            bucket_name: bucket_name.to_string(),
            error,
        }
    })?;
    let bucket_identifier = AppliedIdentifier::new(bucket_name).map_err(|error| {
        AppError::InvalidStateBucketProviderIdentifier {
            bucket_name: bucket_name.to_string(),
            error,
        }
    })?;
    let bucket_id =
        ResourceId::with_provider_identity(provider_name, resource_type, bucket_identity, None);
    match bucket_provider
        .provider()
        .delete(
            &bucket_id,
            &bucket_identifier,
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

async fn load_state_bucket_delete_provider_inputs<E>(
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
    provider_context: &ProviderContext,
) -> Result<ResolvedProviderInputs, AppError> {
    load_and_resolve_provider_configs(
        parsed,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
        ProviderUpstreamScope::ProviderReferences,
    )
    .await
}

async fn create_state_bucket_delete_provider<R>(
    validated_config: &ValidatedConfig<R>,
    provider_configs: &carina_core::parser::ResolvedProviderConfigs,
    provider_name: &str,
) -> Result<carina_core::provider::InitializedProvider, AppError> {
    let ctx = validated_config.context();
    let factory = provider_mod::find_factory(ctx.factories(), provider_name)
        .ok_or_else(|| format!("No provider factory found for '{provider_name}'"))?;
    let config = prepare_default_provider_ready_config(factory, provider_name, provider_configs)?;
    factory
        .create_initialized_provider(None, &config)
        .await
        .map_err(AppError::Provider)
}

/// Run state refresh command
pub async fn run_state_refresh(
    user_dirs: &carina_core::UserDirs,
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
    let parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    let providers = ProviderLoad::load(user_dirs, &parsed.providers, base_dir)?;
    let validated_config = providers.validate_evaluated_config(
        parsed,
        base_dir,
        true,
        &inference_errors,
        &duplicate_declarations,
    )?;

    let verified_backend = verify_for_mutation(
        path,
        validated_config.resources().backend.as_ref(),
        DriftCommand::RefreshState,
    )?;

    // Create backend
    let backend: Box<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?;

    // Acquire lock (unless --lock=false)
    let state_lock = if lock {
        println!("{}", "Acquiring state lock...".cyan());
        let li = backend
            .acquire_lock("refresh")
            .await
            .map_err(|error| map_lock_error(error, path))?;
        println!("  {} Lock acquired", "✓".green());
        StateLock::held(li)
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow()
                .bold()
        );
        StateLock::disabled_by_cli_flag()
    };

    let op_result = run_state_refresh_locked(
        validated_config,
        backend.as_ref(),
        state_lock.as_ref(),
        base_dir,
        provider_context,
        cancel.clone(),
    )
    .await;

    let release_result =
        release_lock_after_execute(backend.as_ref(), state_lock.as_ref(), &cancel).await;
    if state_lock.is_held()
        && release_result.is_ok()
        && matches!(op_result, Err(AppError::Interrupted))
    {
        println!("  {} Lock released", "✓".green());
    }
    op_result?;
    release_result
}

pub(crate) async fn run_state_refresh_locked(
    mut validated_config: ValidatedConfig<carina_core::parser::InferredFile>,
    backend: &dyn StateBackend,
    lock: StateLockRef<'_>,
    base_dir: &std::path::Path,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    // Read current state from backend. carina#3315: persist any older-schema
    // migration under the refresh lock before the "no
    // resources" short-circuit returns — see
    // `apply::load_state_persist_if_migrated`. The on-disk version
    // must advance so the carina#3283 warning text matches reality.
    let mut state_file =
        crate::commands::apply::load_state_persist_if_migrated(backend, lock).await?;
    crate::commands::apply::prune_legacy_data_source_rows_persist_if_changed(
        backend,
        lock,
        &mut state_file,
        &validated_config.resources().data_sources,
    )
    .await?;

    if state_file.as_ref().is_none_or(|s| s.resources().is_empty()) {
        let msg = if state_file.is_none() {
            "No state file found. Nothing to refresh."
        } else {
            "No resources in state. Nothing to refresh."
        };
        println!("{}", msg.yellow());
        return Ok(());
    }

    validated_config.reconcile_for_state_refresh(&state_file);
    let (ctx, parsed) = validated_config.context_and_resources();

    let provider_inputs = load_and_resolve_provider_configs(
        parsed,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
        ProviderUpstreamScope::ProviderReferences,
    )
    .await?;

    let expansion_prelude = crate::wiring::DeferredForExpansionPrelude::new_for_state_refresh(
        parsed,
        &state_file,
        ctx.schemas(),
    )?;
    let sorted_resources = expansion_prelude.sorted_resources();

    // Select provider
    let provider =
        get_provider_with_ctx(ctx, provider_inputs.configs(), ProgressSink::Human).await?;

    println!();
    let progress = ProgressBackend::new(ProgressSink::Human);
    let refresh_phase = RefreshPhase::start(&progress);

    // Read states for all resources using identifier from state.
    // Cancel stops dispatching new reads, then waits for in-flight reads to finish
    // so provider futures are not dropped mid-call.
    let mut managed_current = HashMap::new();
    let mut managed_reads = Vec::new();
    let mut missing_provider_identifier_ids = HashSet::new();
    if let Some(state_file) = state_file.as_ref() {
        for resource in sorted_resources {
            match state_file.build_state_for_resource(&resource.id) {
                Current::Present(present) => {
                    managed_reads.push((resource.id.clone(), present.identifier().clone()));
                }
                Current::MissingIdentifier(state) => {
                    warn_missing_provider_identifier(&resource.id);
                    missing_provider_identifier_ids.insert(resource.id.clone());
                    managed_current.insert(resource.id.clone(), Current::MissingIdentifier(state));
                }
                Current::CreateInFlight(pending) => {
                    managed_current.insert(resource.id.clone(), Current::CreateInFlight(pending));
                }
                Current::CreateOutcomeUnknown(unknown) => {
                    managed_current
                        .insert(resource.id.clone(), Current::CreateOutcomeUnknown(unknown));
                }
                Current::Absent(_state) => {}
            }
        }
    }
    let (managed_states, mut already_refreshed) =
        refresh_existing_resources_until_cancelled(&provider, managed_reads, &cancel).await?;
    managed_current.extend(managed_states);
    match cancel.phase() {
        ShutdownPhase::Running => {}
        ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            return Err(AppError::Interrupted);
        }
    }

    // Build and refresh typed orphans before expansion. A persisted loop
    // child is still classified as an orphan at this point; expansion keeps
    // that ownership intact and the shared prior-state view supplies it to
    // every hydration/lift stage.
    let pre_expansion_desired_ids: HashSet<ResourceId> =
        sorted_resources.iter().map(|r| r.id.clone()).collect();
    let loaded_orphans = state_file
        .as_ref()
        .map(|sf| sf.build_orphans(&pre_expansion_desired_ids))
        .unwrap_or_default();
    let (refreshed_orphans, refreshed_orphan_ids) = refresh_orphans_until_cancelled(
        &provider,
        loaded_orphans,
        state_file.as_ref(),
        ctx.schemas(),
        &cancel,
    )
    .await?;
    for (id, loaded) in refreshed_orphans.iter() {
        match loaded.current() {
            Current::Absent(_state) => {}
            Current::Present(_present) => {}
            Current::CreateInFlight(_pending) => {}
            Current::CreateOutcomeUnknown(_unknown) => {}
            Current::MissingIdentifier(_state) => {
                warn_missing_provider_identifier(id);
                missing_provider_identifier_ids.insert(id.clone());
            }
        }
    }
    let mut orphans = refreshed_orphans.into_plan_orphans();
    already_refreshed.extend(refreshed_orphan_ids);

    // carina#3272: expand `for _, _ in <iter> { ... }` loops the same
    // way `run_apply_locked` does, so the materialised children land
    // in `sorted_resources` (and therefore in the orphan-classification
    // `desired_ids` set below + the `lift_prior_state_enum_leaves`
    // input slice). Without this, every for-loop-produced resource is
    // mis-classified as `(orphan)` on refresh and its enum-typed attrs
    // skip the Enum lift, surfacing snake_case ↔ SCREAMING_CASE
    // as a phantom `~` diff.
    //
    // Unlike plan/apply, refresh uses the prelude's address-preserving typed
    // transition: pending moves and renames remain unapplied. The
    // `already_refreshed` set still prevents redundant child reads.
    // carina#3278: route the expand → child-refresh → hydrate(2nd) →
    // lift quartet through the shared constructor so this path and
    // `run_apply_locked` cannot drift on the sequence again.
    // carina#3739: `build_saved_attrs` returns raw persisted values; consuming
    // them through `lift` is required before the typed expansion input accepts
    // the map for Phase 2.5 hydration.
    crate::wiring::hydrate_prior_managed_states(
        &mut managed_current,
        &mut orphans,
        &provider,
        expansion_prelude.saved_attrs(),
        state_file.as_ref(),
    )
    .await?;
    let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
        .as_ref()
        .map(|sf| {
            sorted_resources
                .iter()
                .filter_map(|r| {
                    let identity = r.id.identity_str()?;
                    let rs = sf.find_resource(&r.id.provider, &r.id.resource_type, identity)?;
                    if rs.dependency_bindings.is_empty() {
                        None
                    } else {
                        Some((r.id.clone(), rs.dependency_bindings.clone()))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let addressed_expansion =
        expansion_prelude.preserve_addresses(crate::wiring::PreserveDeferredForExpansionInputs {
            managed_current,
            orphans,
            already_refreshed,
        });
    let empty_remote_bindings = HashMap::new();
    let data_source_values = addressed_expansion
        .read_data_sources_phase2(AddressedDataSourceReadInputs {
            provider: &provider,
            refresh_phase: Some(&refresh_phase),
            compositions: &parsed.compositions,
            data_sources: &parsed.data_sources,
            remote_bindings: &empty_remote_bindings,
            schemas: ctx.schemas(),
            saved_dep_bindings: &saved_dep_bindings,
            factories: ctx.factories(),
            cancel: Some(&cancel),
        })
        .await?
        .into_values()?;
    match cancel.phase() {
        ShutdownPhase::Running => {}
        ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            return Err(AppError::Interrupted);
        }
    }
    let mut prepared_expansion = addressed_expansion.seal(data_source_values);
    let crate::wiring::ExpandedRefreshState {
        sorted_resources: resorted,
    } = validated_config
        .expand_refresh_and_lift_states(crate::wiring::ValidatedExpandRefreshAndLiftInputs {
            provider: &provider,
            comparison_provider: &provider,
            expansion: &mut prepared_expansion,
            warning_policy: crate::wiring::DeferredForWarningPolicy::Print,
            remote_bindings: &HashMap::new(),
            state_file: &state_file,
            saved_dep_bindings: &saved_dep_bindings,
            refresh_phase: &refresh_phase,
        })
        .await?;
    let (ctx, parsed) = validated_config.context_and_resources();
    let sorted_resources = resorted;
    let crate::wiring::PreparedDeferredForExpansionParts {
        addressed,
        mut data_source_values,
        wait_aliases,
    } = prepared_expansion.into_parts();
    let projected_state = addressed.projected_state(state_file.as_ref());
    let crate::wiring::AddressedStateParts {
        mut managed_current,
        orphans,
        saved_attrs: saved_attrs_for_expansion,
        ..
    } = addressed.into_parts();
    data_source_values = crate::wiring::read_expanded_data_sources_phase2(
        data_source_values,
        crate::wiring::ExpandedDataSourceReadInputs {
            provider: &provider,
            refresh_phase: Some(&refresh_phase),
            managed: &sorted_resources,
            compositions: &parsed.compositions,
            data_sources: &parsed.data_sources,
            managed_current: &managed_current,
            orphans: &orphans,
            remote_bindings: &empty_remote_bindings,
            schemas: ctx.schemas(),
            wait_aliases: &wait_aliases,
            saved_dep_bindings: &saved_dep_bindings,
            factories: ctx.factories(),
            cancel: Some(&cancel),
        },
    )
    .await?
    .into_values()?;
    for (resource, value) in data_source_values.iter() {
        if let DataSourceValue::DeferredToApply(unresolved) = value {
            eprintln!(
                "{}",
                format_deferred_state_refresh_warning(resource, unresolved).yellow()
            );
        }
    }
    match cancel.phase() {
        ShutdownPhase::Running => {}
        ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            return Err(AppError::Interrupted);
        }
    }

    // Expansion can make a pre-expansion orphan desired. Keep orphan metadata
    // typed until this non-planning command needs one unified writeback view.
    let desired_ids: HashSet<ResourceId> = sorted_resources.iter().map(|r| r.id.clone()).collect();
    let orphan_ids: Vec<ResolvedResourceId> = orphans
        .keys()
        .filter(|id| !desired_ids.contains(*id))
        .cloned()
        .map(ResolvedResourceId::new)
        .collect();
    managed_current = into_all_prior_states(orphans, managed_current);

    // Restore managed attributes from the same lifted map used by the
    // expansion hydration above (CloudControl doesn't always return them).
    // Data sources have no persisted rows and therefore hydrate against an
    // empty saved-attribute set even when a managed orphan shares their ID.
    let saved_attrs = saved_attrs_for_expansion;
    crate::wiring::hydrate_prior_data_source_values(
        &mut managed_current,
        &mut data_source_values,
        &mut Orphans::new(),
        &provider,
        &saved_attrs,
        &projected_state,
    )
    .await?;
    crate::wiring::lift_data_source_value_enum_leaves(
        &mut data_source_values,
        &parsed.data_sources,
        ctx.schemas(),
    );
    // awscc#251: also lift the provider-read managed current states (not just
    // `saved_attrs`) — the values read at the refresh loop above arrive
    // as plain `String` for IAM enum fields and must be lifted before
    // they are written back / compared.
    // carina#3272: same `sorted_resources` reason as above.
    lift_prior_state_enum_leaves(
        &mut managed_current,
        &mut Orphans::new(),
        &sorted_resources,
        ctx.schemas(),
    );

    let resolved_resources = ResolvedResource::collect_resolved(sorted_resources.iter().cloned())?;

    let mut state = state_file.take().unwrap();

    drop(refresh_phase);

    let mut refresh_counts = StateRefreshCounts::default();

    for resource in &resolved_resources {
        if missing_provider_identifier_ids.contains(&resource.id) {
            continue;
        }
        let fresh_current = match managed_current.get(&resource.id) {
            Some(current) => current,
            None => continue, // Not in state, skip
        };
        diff_display_update_resource_with_provider(
            &resource.resolved_id(),
            fresh_current,
            &mut state,
            Some(resource),
            ctx.schemas(),
            &provider,
            "",
            &mut refresh_counts,
        )?;
    }

    // Process orphaned resources (in state but removed from config)
    for orphan_id in &orphan_ids {
        if missing_provider_identifier_ids.contains(orphan_id.as_inner()) {
            continue;
        }
        let fresh_current = match managed_current.get(orphan_id.as_inner()) {
            Some(current) => current,
            None => continue,
        };
        diff_display_update_resource_with_provider(
            orphan_id,
            fresh_current,
            &mut state,
            None,
            ctx.schemas(),
            &provider,
            " (orphan)",
            &mut refresh_counts,
        )?;
    }

    let deposed_summary = refresh_deposed_generations_until_cancelled(
        &provider,
        &mut state,
        &resolved_resources,
        ctx.schemas(),
        &cancel,
    )
    .await?;
    match cancel.phase() {
        ShutdownPhase::Running => {}
        ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            return Err(AppError::Interrupted);
        }
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
        // `ResourceRef` snapshots that post-apply export resolution
        // re-resolution needs (#3169 / #3177).
        let post_apply_states =
            crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
                &managed_current,
                &data_source_values,
                &state,
            );
        let checked_exports = carina_core::validation::check_export_values_pre_apply(
            &parsed.export_params,
            &carina_core::binding_index::ResolvedBindings::for_validation(parsed),
            provider_context,
        )
        .map_err(AppError::Validation)?;
        let values = crate::commands::shared::state_writeback::resolve_export_values_post_apply(
            crate::commands::shared::state_writeback::ResolveExportValuesPostApplyInput {
                export_params: &checked_exports,
                sorted_resources: &sorted_resources,
                pre_resolve_compositions: &parsed.compositions,
                post_apply_states: &post_apply_states,
                schemas: ctx.schemas(),
                wait_aliases: &wait_aliases,
                provider_context,
            },
        )?;
        let resolution = crate::commands::shared::state_writeback::materialize_exports(&values)?;
        resolution.write_into(&mut state)
    } else {
        SkippedExports::default()
    };

    // Save state (with or without lock validation)
    match lock {
        StateLockRef::Held(lock) => {
            crate::commands::apply::save_state_locked(backend, lock, &mut state).await?
        }
        StateLockRef::Disabled(disabled) => {
            crate::commands::apply::save_state_unlocked(backend, disabled, &mut state).await?
        }
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
    reads: Vec<(ResourceId, AppliedIdentifier)>,
    cancel: &ShutdownToken,
) -> Result<(HashMap<ResourceId, Current>, HashSet<ResourceId>), AppError> {
    let mut managed_current = HashMap::new();
    let mut refreshed = HashSet::new();
    let mut read_iter = reads.into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut refresh_cancelled = false;
    let cleanup_loop = cancel.cleanup_aware_loop();

    loop {
        let cleanup_wait = match cleanup_loop.step() {
            LoopStep::Continue(wait) => wait,
            LoopStep::Abandon => {
                refresh_cancelled = true;
                break;
            }
        };
        match cleanup_wait.phase() {
            LoopShutdownPhase::Running => {}
            LoopShutdownPhase::Graceful => refresh_cancelled = true,
        }

        while !refresh_cancelled && in_flight.len() < 5 {
            let Some((id, identifier)) = read_iter.next() else {
                break;
            };
            in_flight.push(async move {
                let fresh_state = provider
                    .read(&id, &identifier, carina_core::provider::ReadRequest)
                    .await
                    .map_err(AppError::Provider)?
                    .into_state();
                Ok((id, fresh_state))
            });
        }

        if in_flight.is_empty() {
            break;
        }

        let draining = refresh_cancelled;
        #[cfg(test)]
        let next_in_flight = async {
            if draining {
                crate::commands::shared::cancellation_test_support::observe_refresh_drain_wait(
                    cancel,
                )
                .await;
            }
            in_flight.next().await
        };
        #[cfg(not(test))]
        let next_in_flight = in_flight.next();
        // A ready provider result is harvested before cleanup priority. If
        // refresh is already cancelled, the short-circuit below discards it.
        let next = cleanup_wait.until_cleanup_priority(next_in_flight);
        tokio::pin!(next);
        let next = if draining {
            next.await
        } else {
            tokio::select! {
                biased;
                next = &mut next => next,
                _ = cancel.cancelled() => {
                    refresh_cancelled = true;
                    continue;
                }
            }
        };
        let result: Result<(ResourceId, State), AppError> = match next {
            CleanupInterrupted::Completed(Some(result)) => result,
            CleanupInterrupted::Completed(None) => break,
            CleanupInterrupted::Abandoned => {
                refresh_cancelled = true;
                break;
            }
        };

        if refresh_cancelled {
            continue;
        }

        let (id, state) = result?;
        refreshed.insert(id.clone());
        managed_current.insert(id, Current::from_state(state));
    }

    drop(in_flight);
    drop(read_iter);

    if refresh_cancelled {
        return Err(AppError::Interrupted);
    }

    Ok((managed_current, refreshed))
}

async fn refresh_orphans_until_cancelled(
    provider: &dyn Provider,
    loaded: LoadedOrphans,
    state_file: Option<&StateFile>,
    schemas: &carina_core::schema::SchemaRegistry,
    cancel: &ShutdownToken,
) -> Result<(LoadedOrphans, HashSet<ResourceId>), AppError> {
    let mut entries = loaded.into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut refreshed_orphans = LoadedOrphans::default();
    let mut refreshed = HashSet::new();
    let mut refresh_cancelled = false;
    let cleanup_loop = cancel.cleanup_aware_loop();

    loop {
        let cleanup_wait = match cleanup_loop.step() {
            LoopStep::Continue(wait) => wait,
            LoopStep::Abandon => {
                refresh_cancelled = true;
                break;
            }
        };
        match cleanup_wait.phase() {
            LoopShutdownPhase::Running => {}
            LoopShutdownPhase::Graceful => refresh_cancelled = true,
        }

        while !refresh_cancelled && in_flight.len() < 5 {
            let Some((id, loaded)) = entries.next() else {
                break;
            };
            let provider_read = match loaded.current() {
                Current::Absent(_state) => false,
                Current::Present(_present) => true,
                Current::CreateInFlight(_pending) => false,
                Current::CreateOutcomeUnknown(_unknown) => false,
                Current::MissingIdentifier(_state) => false,
            };
            in_flight.push(async move {
                let loaded =
                    refresh_loaded_orphan_with_retry(provider, loaded, state_file, schemas)
                        .await
                        .map_err(AppError::Provider)?;
                Ok((id, loaded, provider_read))
            });
        }

        if in_flight.is_empty() {
            break;
        }

        let draining = refresh_cancelled;
        #[cfg(test)]
        let next_in_flight = async {
            if draining {
                crate::commands::shared::cancellation_test_support::observe_refresh_drain_wait(
                    cancel,
                )
                .await;
            }
            in_flight.next().await
        };
        #[cfg(not(test))]
        let next_in_flight = in_flight.next();
        let next = cleanup_wait.until_cleanup_priority(next_in_flight);
        tokio::pin!(next);
        let next = if draining {
            next.await
        } else {
            tokio::select! {
                biased;
                next = &mut next => next,
                _ = cancel.cancelled() => {
                    refresh_cancelled = true;
                    continue;
                }
            }
        };
        let result: Result<(ResourceId, Orphan, bool), AppError> = match next {
            CleanupInterrupted::Completed(Some(result)) => result,
            CleanupInterrupted::Completed(None) => break,
            CleanupInterrupted::Abandoned => {
                refresh_cancelled = true;
                break;
            }
        };

        if refresh_cancelled {
            continue;
        }
        let (id, loaded, provider_read) = result?;
        if provider_read {
            refreshed.insert(id);
        }
        refreshed_orphans.insert(loaded);
    }

    drop(in_flight);
    drop(entries);
    if refresh_cancelled {
        return Err(AppError::Interrupted);
    }
    Ok((refreshed_orphans, refreshed))
}

fn warn_missing_provider_identifier(id: &ResourceId) {
    eprintln!(
        "{}",
        format!(
            "Warning: {id}: saved state has no provider identifier and was not refreshed; the row was preserved. Add a `removed` block and apply it, then add an `import` block with the real provider identifier to restore management."
        )
        .yellow()
    );
}

#[derive(Clone)]
struct DeposedRefreshTarget {
    row_provider: String,
    row_resource_type: String,
    row_identity: ResourceIdentity,
    row_provider_instance: Option<String>,
    id: ResolvedResourceId,
    generation: DeposedInstance,
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
    desired_resources: &[ResolvedResource],
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
    let cleanup_loop = cancel.cleanup_aware_loop();

    for target in targets {
        let cleanup_wait = match cleanup_loop.step() {
            LoopStep::Continue(wait) => wait,
            LoopStep::Abandon => return Err(AppError::Interrupted),
        };
        match cleanup_wait.phase() {
            LoopShutdownPhase::Running => {}
            LoopShutdownPhase::Graceful => return Err(AppError::Interrupted),
        }

        let identifier = match AppliedIdentifier::new(target.generation.identifier.as_str()) {
            Ok(identifier) => identifier,
            Err(_) => {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: {} {}: saved deposed generation {} has no provider identifier and was not refreshed; the generation was preserved. Apply a matching `removed` block to abandon it from state.",
                        target.id,
                        deposed_state_marker(&target.generation.key, &target.generation.identifier),
                        target.generation.key,
                    )
                    .yellow()
                );
                summary.failed_generations += 1;
                continue;
            }
        };

        let read_result = match cleanup_wait
            .until_cleanup_priority(provider.read(
                &target.id,
                &identifier,
                carina_core::provider::ReadRequest,
            ))
            .await
        {
            CleanupInterrupted::Completed(result) => result,
            CleanupInterrupted::Abandoned => return Err(AppError::Interrupted),
        };
        match cancel.phase() {
            ShutdownPhase::Running => {}
            ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
                return Err(AppError::Interrupted);
            }
        }

        let fresh_state = match read_result {
            Ok(outcome) => outcome.into_state(),
            Err(err) => {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: failed refreshing {}.{} {} {}: {}",
                        target.row_provider,
                        target.row_resource_type,
                        target.row_identity,
                        deposed_state_marker(&target.generation.key, &target.generation.identifier),
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
                target.row_identity.as_str(),
                &target.generation.key,
            );
            summary.removed_generations += 1;
            println!(
                "  {} \"{}\" {}:",
                target.id.display_type().cyan(),
                target.id.identity_display(),
                deposed_state_marker(&target.generation.key, &target.generation.identifier)
            );
            println!("    {} resource no longer exists", "-".red());
            println!();
            continue;
        }

        let desired_resource = desired_resource_for_deposed(desired_resources, &target);
        let masking_resource = desired_resource
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
        .await?;
        match cancel.phase() {
            ShutdownPhase::Running => {}
            ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
                return Err(AppError::Interrupted);
            }
        }
        let schema = schemas
            .get_for(&masking_resource)
            .map(|schema| schema.schema().as_schema());
        let attributes = target
            .generation
            .refreshed_attributes_for_resource_and_schema(
                &masking_resource,
                schema,
                &fresh_state.attributes,
            );

        if attributes.as_persisted() == target.generation.persisted_attributes() {
            continue;
        }
        summary.updated_generations += 1;

        let updated = target
            .generation
            .clone()
            .with_refreshed_attributes(attributes);
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
            target.id.identity_display(),
            deposed_state_marker(&target.generation.key, &target.generation.identifier)
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
                let identity = row.identity.clone();
                let id = ResolvedResourceId::with_provider_identity(
                    row.provider.clone(),
                    row.resource_type.clone(),
                    identity.clone(),
                    deposed.provider_instance.clone(),
                );
                DeposedRefreshTarget {
                    row_provider: row.provider.clone(),
                    row_resource_type: row.resource_type.clone(),
                    row_identity: identity,
                    row_provider_instance: row.directives.provider_instance.clone(),
                    id,
                    generation: deposed.clone(),
                }
            })
        })
        .collect()
}

fn desired_resource_for_deposed<'a>(
    desired_resources: &'a [ResolvedResource],
    target: &DeposedRefreshTarget,
) -> Option<&'a ResolvedResource> {
    desired_resources.iter().find(|resource| {
        resource.id.provider == target.row_provider
            && resource.id.resource_type == target.row_resource_type
            && resource.id.identity_str() == Some(target.row_identity.as_str())
            && resource.id.provider_instance == target.generation.provider_instance
    })
}

fn synthetic_deposed_resource(target: &DeposedRefreshTarget) -> ResolvedResource {
    let mut resource = ResolvedResource::from_id(target.id.clone());
    for (key, value) in target.generation.persisted_attributes().to_values() {
        resource.set_attr(key, value);
    }
    resource
}

async fn normalize_deposed_read_state<P>(
    provider: &P,
    target: &DeposedRefreshTarget,
    resource: &ResolvedResource,
    fresh_state: &mut State,
    schemas: &carina_core::schema::SchemaRegistry,
) -> Result<(), ProviderError>
where
    P: ProviderNormalizer + ?Sized,
{
    let mut states = HashMap::from([(target.id.as_inner().clone(), fresh_state.clone())]);
    let saved_attrs = deposed_saved_attrs(target).lift(schemas);
    carina_core::provider::hydrate_provider_states(
        provider,
        &mut states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await?;

    if let Some(normalized) = states.remove(target.id.as_inner()) {
        *fresh_state = normalized;
    }
    if let Some(schema) = schemas.get_for(resource.as_resource()) {
        carina_core::utils::lift_state_enum_leaves(&mut fresh_state.attributes, schema.as_schema());
    }
    Ok(())
}

fn deposed_saved_attrs(target: &DeposedRefreshTarget) -> RawSavedAttrs {
    RawSavedAttrs::from_persisted(
        HashMap::from([(
            target.id.as_inner().clone(),
            target.generation.persisted_attributes().clone(),
        )]),
        // Deposed generations predate per-instance nested write-only provenance.
        HashMap::new(),
    )
}

/// Compare old state with fresh provider state for a single resource,
/// display any changes, and update the state file accordingly.
///
/// When `resource` is `Some`, current desired metadata such as directives and
/// dependencies is rebuilt. Authoring for desired keys is realigned to the
/// fresh values, while live pending removals at any authored depth are carried
/// from the prior record. Ambiguous list-element removals remain whole-value
/// comparisons rather than being assigned to guessed elements.
/// When `None`, the existing orphan row keeps every host-owned field and only
/// its provider-observed state is replaced.
///
/// `label_suffix` is appended to the resource header (e.g., `" (orphan)"`).
#[derive(Default)]
struct StateRefreshCounts {
    updated: u32,
    unchanged: u32,
}

#[allow(clippy::too_many_arguments)]
fn diff_display_update_resource_with_provider(
    id: &ResolvedResourceId,
    fresh_current: &Current,
    state: &mut carina_state::StateFile,
    resource: Option<&ResolvedResource>,
    schemas: &carina_core::schema::SchemaRegistry,
    provider: &ProviderRouter,
    label_suffix: &str,
    counts: &mut StateRefreshCounts,
) -> Result<(), AppError> {
    let identity = id.identity();
    let existing = state.find_resource(&id.provider, &id.resource_type, id.identity_str());
    let existing_rs = match existing {
        Some(rs) => rs,
        None => return Ok(()),
    };
    let (fresh_state, exists) = match StateCommandCurrent::from(fresh_current) {
        StateCommandCurrent::State { state, kind } => {
            let exists = match kind {
                StateCommandStateKind::Absent => false,
                StateCommandStateKind::Present | StateCommandStateKind::MissingIdentifier => true,
            };
            (state, exists)
        }
        StateCommandCurrent::CreateInFlight(pending) => {
            counts.unchanged += 1;
            println!(
                "  {} \"{}\"{}:",
                id.display_type().cyan(),
                identity,
                label_suffix,
            );
            println!("    {}", create_in_flight_status(pending));
            println!();
            return Ok(());
        }
        StateCommandCurrent::CreateOutcomeUnknown(unknown) => {
            counts.unchanged += 1;
            println!(
                "  {} \"{}\"{}:",
                id.display_type().cyan(),
                identity,
                label_suffix,
            );
            println!("    {}", create_outcome_unknown_status(unknown));
            println!();
            return Ok(());
        }
    };

    let resolved_comparison = provider.comparison_for(schemas, id.as_inner())?;
    let owner_schema = resolved_comparison.owner_schema();
    let comparison = resolved_comparison.context();
    let resource_schema = Some(owner_schema.schema().as_schema());

    let refreshed_resource_state = if exists {
        match resource {
            Some(resource) => Some(
                ResourceState::from_provider_state_for_resource_and_schema(
                    resource,
                    fresh_state,
                    Some(existing_rs),
                    resource_schema,
                    RowBasis::Carried,
                    None,
                    comparison,
                )
                .map_err(|source| {
                    AppError::from_resource_state_encoding(resource.id.clone(), source)
                })?,
            ),
            None => {
                let refreshable = existing_rs.refreshable().ok_or_else(|| {
                    AppError::Config(format!(
                        "Cannot refresh in-flight resource state for {}",
                        id.as_inner()
                    ))
                })?;
                Some(
                    refreshable
                        .refreshed_from_provider_state(fresh_state, resource_schema)
                        .map_err(|source| {
                            AppError::from_resource_state_encoding(id.as_inner().clone(), source)
                        })?,
                )
            }
        }
    } else {
        None
    };

    // Build old attributes as DSL values for comparison
    let old_attrs = existing_rs
        .persisted_attributes()
        .map(PersistedAttributes::to_values)
        .unwrap_or_default();
    let refreshed_attrs: HashMap<String, Value> = refreshed_resource_state
        .as_ref()
        .and_then(ResourceState::persisted_attributes)
        .map(PersistedAttributes::to_values)
        .unwrap_or_default();

    let mut has_changes = false;
    let mut changes: Vec<String> = Vec::new();

    if !exists {
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
                (Some(old), Some(new))
                    if !carina_core::diff_helpers::resource_attribute_values_equal(
                        old,
                        new,
                        Some(owner_schema.schema()),
                        key,
                        comparison,
                    ) =>
                {
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
            identity,
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
        state.remove_resource(&id.provider, &id.resource_type, identity.as_str());
    }

    Ok(())
}

#[cfg(test)]
fn diff_display_update_resource(
    id: &ResolvedResourceId,
    fresh_current: &Current,
    state: &mut carina_state::StateFile,
    resource: Option<&ResolvedResource>,
    schemas: &carina_core::schema::SchemaRegistry,
    label_suffix: &str,
    counts: &mut StateRefreshCounts,
) -> Result<(), AppError> {
    let (schemas, provider) =
        carina_core::provider::comparison_fixture_for_test(schemas, std::iter::once(id.as_inner()));
    diff_display_update_resource_with_provider(
        id,
        fresh_current,
        state,
        resource,
        &schemas,
        &provider,
        label_suffix,
        counts,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::shared::cancellation_test_support::{
        CancellationFixtureBase, MOCK_PROVIDER_ENV_LOCK, ScopedEnv, install_refresh_drain_barrier,
    };
    use crate::provider_upstream_test_support::{
        ProviderConfigCaptureFactory, provider_default_tags_local_resource_fixture,
        provider_default_tags_missing_upstream_fixture, provider_upstream_fixture,
    };
    use carina_core::parser::parse_file;
    use carina_core::parser::{CompareOp, ValidateExpr};
    use carina_core::provider::{
        BoxFuture, CreateRequest, DeleteRequest, ProviderError, ProviderResult, ReadRequest, Sent,
        UpdateRequest,
    };
    use carina_core::resource::{
        ConcreteValue, DeferredValue, LiveComposition, LiveCompositionArgument,
        LiveCompositionSignature, ModuleConstraintId, PendingModuleConstraint, Resource,
    };
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    use carina_state::{DeposedInstance, DeposedKey};
    use indexmap::IndexMap;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    const LEGACY_SECRET_PREFIX: &str = "_secret:argon2:";

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn saved_composition_argument(value: Value) -> LiveCompositionArgument {
        LiveCompositionArgument::from_value(
            value,
            carina_core::parser::TypeExpr::String,
            carina_core::parser::ArgumentOrigin::Defaulted,
        )
    }

    fn structural_legacy_secret(hash_hex: &str) -> serde_json::Value {
        json!({
            carina_core::value::PERSISTED_SECRET_KEY:
                format!("argon2-legacy:73616c74:{hash_hex}"),
        })
    }

    fn structural_secret_digest(value: &serde_json::Value) -> &str {
        let object = value.as_object().expect("secret is a structural object");
        assert_eq!(object.len(), 1);
        object
            .get(carina_core::value::PERSISTED_SECRET_KEY)
            .and_then(serde_json::Value::as_str)
            .expect("secret tag carries a digest")
    }

    #[test]
    fn state_refresh_counts_reference_form_equivalent_value_as_unchanged() {
        use carina_core::provider::{InitializedProvider, ProviderRouter, ReferenceScope};
        use carina_core::schema::{
            ReferenceForm, ReferenceForms, ReferenceTemplate, TypeIdentity, ValidatedConfigSchema,
            ValidatedProviderSchemaWitness,
        };

        let identity = TypeIdentity::new(Some("aws"), ["lambda", "Function"], "Name");
        let schema = ResourceSchema::new("lambda.Permission").attribute(
            AttributeSchema::optional(
                "function_name",
                AttributeType::refined_string(Some(identity.clone()), None, None, None),
            )
            .build(),
        );
        let forms = ReferenceForms::from([(
            identity,
            ReferenceForm::Template {
                long: ReferenceTemplate::new(
                    "arn:{partition}:lambda:{region}:{account}:function:{name}",
                )
                .expect("valid reference template"),
            },
        )]);
        let witness = ValidatedProviderSchemaWitness::new(
            vec![schema.validate().expect("valid schema")],
            ValidatedConfigSchema::default(),
            forms,
        )
        .expect("valid provider schema witness");
        let mut schemas = SchemaRegistry::new();
        schemas
            .insert_provider_witness("aws", &witness)
            .expect("fixture owner has one validated witness");

        let desired = resolved(
            Resource::with_provider("aws", "lambda.Permission", "invoke", None).with_attribute(
                "function_name",
                Value::Concrete(ConcreteValue::String("processor".into())),
            ),
        );
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("lambda.Permission", "invoke", "aws", "permission-id")
                    .expect("fixture identifier is non-empty")
                    .with_attribute(
                        "function_name",
                        json!("arn:aws:lambda:us-east-1:123456789012:function:processor"),
                    ),
            )
            .expect("state fixture is valid");
        let fresh = Current::from_state(
            State::existing(
                desired.id.clone(),
                HashMap::from([(
                    "function_name".to_string(),
                    Value::Concrete(ConcreteValue::String("processor".into())),
                )]),
            )
            .with_identifier("permission-id"),
        );
        let mut provider = ProviderRouter::new();
        provider.add_provider(
            "aws".to_string(),
            InitializedProvider::for_test(
                Box::new(carina_provider_mock::MockProvider::new()),
                ReferenceScope::available("aws", "us-east-1", "123456789012")
                    .expect("fixture scope is valid"),
            ),
        );
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource_with_provider(
            &ResolvedResourceId::new(desired.id.clone()),
            &fresh,
            &mut state,
            Some(&desired),
            &schemas,
            &provider,
            "",
            &mut counts,
        )
        .expect("state refresh comparison succeeds");

        assert_eq!(counts.updated, 0);
        assert_eq!(counts.unchanged, 1);
    }

    struct ModuleConstraintRefreshBackend {
        state: StateFile,
    }

    fn non_empty_refresh_backend(provider: &str) -> ModuleConstraintRefreshBackend {
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("test.resource", "orphan", provider, "orphan")
                    .expect("fixture identifier is non-empty"),
            )
            .unwrap();
        ModuleConstraintRefreshBackend { state }
    }

    #[async_trait::async_trait]
    impl StateBackend for ModuleConstraintRefreshBackend {
        async fn read_state(
            &self,
        ) -> carina_state::BackendResult<Option<carina_state::LoadedState>> {
            Ok(Some(carina_state::LoadedState::Pristine(
                self.state.clone(),
            )))
        }

        async fn read_state_bytes(
            &self,
        ) -> carina_state::BackendResult<Option<carina_state::RawState>> {
            carina_state::RawState::from_state(&self.state, "module constraint test state")
                .map(Some)
        }

        async fn write_state(&self, _state: &StateFile) -> carina_state::BackendResult<()> {
            Ok(())
        }

        async fn acquire_lock(&self, operation: &str) -> carina_state::BackendResult<LockInfo> {
            Ok(LockInfo::for_test(operation))
        }

        async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
            Ok(())
        }

        async fn renew_lock(&self, lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
            Ok(lock.renewed_for_test())
        }

        async fn write_state_locked(
            &self,
            state: &StateFile,
            _lock: &LockInfo,
        ) -> carina_state::BackendResult<()> {
            self.write_state(state).await
        }

        async fn force_unlock(&self, _lock_id: &str) -> carina_state::BackendResult<()> {
            Ok(())
        }

        async fn bucket_exists(&self) -> carina_state::BackendResult<bool> {
            Ok(true)
        }

        async fn create_bucket(&self) -> carina_state::BackendResult<()> {
            Ok(())
        }

        fn resource_type(&self) -> Option<&str> {
            None
        }

        fn provider_name(&self) -> Option<&str> {
            None
        }

        fn resource_definition(&self, _bucket_name: &str) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn state_refresh_resolves_provider_config_from_upstream_state_before_construction() {
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        let received_role_arn = Arc::new(Mutex::new(None));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            })],
            &provider_configs,
        );
        let backend = non_empty_refresh_backend("test");

        run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            &downstream,
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("state refresh must construct the provider from resolved upstream attributes");

        assert_eq!(
            *received_role_arn.lock().unwrap(),
            Some(Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:role/upstream".into()
            )))
        );
    }

    #[tokio::test]
    async fn state_refresh_provider_resolution_ignores_unrelated_missing_upstream_state() {
        let (_temp, downstream, mut parsed) = provider_upstream_fixture();
        let unrelated = downstream.parent().unwrap().join("unrelated");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(
            unrelated.join("main.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .unwrap();
        parsed
            .upstream_states
            .push(carina_core::parser::UpstreamState {
                binding: "unrelated".to_string(),
                source: PathBuf::from("../unrelated"),
            });
        let received_role_arn = Arc::new(Mutex::new(None));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(ProviderConfigCaptureFactory { received_role_arn })],
            &provider_configs,
        );
        let backend = non_empty_refresh_backend("test");
        run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            &downstream,
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("state refresh must not load an upstream unused by provider configuration");
    }

    #[tokio::test]
    async fn state_refresh_provider_resolution_reports_referenced_missing_upstream_state() {
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        std::fs::remove_file(
            downstream
                .parent()
                .unwrap()
                .join("upstream")
                .join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
        )
        .unwrap();
        let received_role_arn = Arc::new(Mutex::new(None));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(ProviderConfigCaptureFactory { received_role_arn })],
            &provider_configs,
        );
        let backend = non_empty_refresh_backend("test");

        let error = match run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            &downstream,
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        {
            Ok(_) => panic!("state refresh must fail when provider-referenced state is missing"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("upstream_state 'mgmt'"), "{message}");
        assert!(message.contains("no state found"), "{message}");
        assert!(
            message.contains("required by provider 'test' attribute 'role_arn'"),
            "{message}"
        );
        let AppError::ProviderReferencedUpstream { error: source, .. } = &error else {
            panic!("provider-reference context must remain a typed AppError: {error:?}");
        };
        assert!(
            matches!(source.as_ref(), AppError::Config(_)),
            "the wrapped source must remain the original typed upstream error: {source:?}"
        );
        assert_eq!(message.matches("no state found").count(), 1, "{message}");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[tokio::test]
    async fn state_refresh_without_downstream_state_ignores_missing_provider_upstream() {
        let user_dirs = carina_core::TestUserDirs::new();
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        std::fs::remove_file(
            downstream
                .parent()
                .unwrap()
                .join("upstream")
                .join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
        )
        .unwrap();
        let backend =
            crate::commands::local_backend_for_test(downstream.join("missing-local-state.json"));
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, &downstream).unwrap();
        let validated_config = providers
            .validate_evaluated_config(parsed, &downstream, true, &[], &[])
            .unwrap();

        run_state_refresh_locked(
            validated_config,
            backend.as_ref(),
            StateLock::disabled_for_test().as_ref(),
            &downstream,
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("state refresh must return before resolving providers when no local state exists");
    }

    #[tokio::test]
    async fn state_refresh_allows_default_tags_referencing_local_computed_attributes() {
        let user_dirs = carina_core::TestUserDirs::new();
        let (_temp, base_dir, parsed) = provider_default_tags_local_resource_fixture();
        let backend = non_empty_refresh_backend("mock");
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, &base_dir).unwrap();
        let validated_config = providers
            .validate_evaluated_config(parsed, &base_dir, true, &[], &[])
            .unwrap();

        run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            &base_dir,
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("state refresh must defer provider default tags until resource normalization");
    }

    #[tokio::test]
    async fn state_refresh_ignores_missing_upstream_referenced_only_by_default_tags() {
        let user_dirs = carina_core::TestUserDirs::new();
        let (_temp, base_dir, parsed) = provider_default_tags_missing_upstream_fixture();
        let backend = non_empty_refresh_backend("mock");
        let providers = ProviderLoad::load(&user_dirs, &parsed.providers, &base_dir).unwrap();
        let validated_config = providers
            .validate_evaluated_config(parsed, &base_dir, true, &[], &[])
            .unwrap();

        run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            &base_dir,
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("state refresh must not load upstream state used only by default tags");
    }

    #[tokio::test]
    async fn state_bucket_delete_prepares_resolved_upstream_provider_config() {
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        let inputs = load_state_bucket_delete_provider_inputs(
            &parsed,
            &downstream,
            &ProviderContext::default(),
        )
        .await
        .expect("state bucket delete provider inputs must resolve");
        let received_role_arn = Arc::new(Mutex::new(None));
        let ctx = ValidatedConfig::from_factories_for_test(
            (),
            vec![Box::new(ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            })],
            inputs.configs().as_provider_configs(),
        );
        create_state_bucket_delete_provider(&ctx, inputs.configs(), "test")
            .await
            .expect("state bucket delete must create from the resolved value");

        assert_eq!(
            received_role_arn.lock().unwrap().as_ref(),
            Some(&Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:role/upstream".into()
            )))
        );
    }

    #[tokio::test]
    async fn state_bucket_delete_reports_provider_attribute_for_missing_upstream_state() {
        let (_temp, downstream, parsed) = provider_upstream_fixture();
        std::fs::remove_file(
            downstream
                .parent()
                .unwrap()
                .join("upstream")
                .join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
        )
        .unwrap();

        let error = match load_state_bucket_delete_provider_inputs(
            &parsed,
            &downstream,
            &ProviderContext::default(),
        )
        .await
        {
            Ok(_) => panic!("bucket delete must fail when provider-referenced state is missing"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("upstream_state 'mgmt'"), "{message}");
        assert!(message.contains("no state found"), "{message}");
        assert!(
            message.contains("required by provider 'test' attribute 'role_arn'"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn state_bucket_delete_ignores_upstream_referenced_only_by_default_tags() {
        let (_temp, base_dir, mut parsed) = provider_default_tags_missing_upstream_fixture();
        parsed.providers[0].name = "test".to_string();
        let inputs = load_state_bucket_delete_provider_inputs(
            &parsed,
            &base_dir,
            &ProviderContext::default(),
        )
        .await
        .expect("bucket delete must not load tag-only upstream state");
        let received_role_arn = Arc::new(Mutex::new(None));
        let ctx = ValidatedConfig::from_factories_for_test(
            (),
            vec![Box::new(ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            })],
            inputs.configs().as_provider_configs(),
        );

        create_state_bucket_delete_provider(&ctx, inputs.configs(), "test")
            .await
            .expect("bucket delete must construct without resolving default tags");
        assert_eq!(*received_role_arn.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn state_bucket_delete_accepts_parser_deferred_non_literal_default_tags() {
        let temp = tempfile::tempdir().unwrap();
        let parsed = parse_file(
            r#"
                let tags_mod = use { source = "./modules/standard-tags" }
                let tags = tags_mod { environment = "dev" }

                provider test {
                    default_tags = tags.tags
                }
            "#,
            std::path::Path::new("state_bucket_delete.crn"),
            &ProviderContext::default(),
        )
        .expect("fixture must parse");
        assert!(
            parsed.providers[0]
                .unresolved_attributes
                .contains_key("default_tags"),
            "the fixture must exercise the parser shape used by bucket delete"
        );

        let inputs = load_state_bucket_delete_provider_inputs(
            &parsed,
            temp.path(),
            &ProviderContext::default(),
        )
        .await
        .expect("deferred default tags must not block bucket provider construction");
        let received_role_arn = Arc::new(Mutex::new(None));
        let ctx = ValidatedConfig::from_factories_for_test(
            (),
            vec![Box::new(ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            })],
            inputs.configs().as_provider_configs(),
        );

        create_state_bucket_delete_provider(&ctx, inputs.configs(), "test")
            .await
            .expect("bucket delete must use the real prepared-provider path");
        assert_eq!(*received_role_arn.lock().unwrap(), None);
    }

    struct ModuleConstraintRefreshFactory {
        reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct ModuleConstraintRefreshProvider {
        reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct CancellingDataSourceRefreshFactory {
        reads: Arc<std::sync::atomic::AtomicUsize>,
        trigger: carina_core::shutdown::GracefulShutdownTrigger,
    }

    struct CancellingDataSourceRefreshProvider {
        reads: Arc<std::sync::atomic::AtomicUsize>,
        trigger: carina_core::shutdown::GracefulShutdownTrigger,
    }

    impl carina_core::provider::ProviderFactory for ModuleConstraintRefreshFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "state refresh module constraint provider"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "local".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
            _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
            let reads = self.reads.clone();
            Box::pin(async move {
                Ok(carina_core::provider::InitializeOutcome::unavailable()
                    .with_provider(Box::new(ModuleConstraintRefreshProvider { reads })))
            })
        }

        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = vec![
                ResourceSchema::new("test.resource"),
                ResourceSchema::new("orphan"),
                ResourceSchema::new("lookup").as_data_source(),
            ];
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    impl Provider for ModuleConstraintRefreshProvider {
        fn name(&self) -> &str {
            "state-refresh-module-constraint"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
            let id = id.clone();
            Box::pin(async move { Ok(carina_core::provider::ReadOutcome::not_found(id)) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
        > {
            Box::pin(
                async move { Err(attempt.untracked(ProviderError::internal("unexpected create"))) },
            )
        }

        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            Box::pin(async { panic!("unexpected resume_create") })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unexpected update")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    impl carina_core::provider::ProviderFactory for CancellingDataSourceRefreshFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "cancelling state refresh provider"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "local".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
            _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
            let reads = self.reads.clone();
            let trigger = self.trigger.clone();
            Box::pin(async move {
                Ok(
                    carina_core::provider::InitializeOutcome::unavailable().with_provider(
                        Box::new(CancellingDataSourceRefreshProvider { reads, trigger }),
                    ),
                )
            })
        }

        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = vec![
                ResourceSchema::new("test.resource"),
                ResourceSchema::new("lookup").as_data_source(),
            ];
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    impl Provider for CancellingDataSourceRefreshProvider {
        fn name(&self) -> &str {
            "cancelling-state-refresh"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
            let id = id.clone();
            Box::pin(async move { Ok(carina_core::provider::ReadOutcome::not_found(id)) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let call = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                self.trigger.request_graceful_shutdown();
            }
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
        > {
            Box::pin(
                async move { Err(attempt.untracked(ProviderError::internal("unexpected create"))) },
            )
        }

        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            Box::pin(async { panic!("unexpected resume_create") })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unexpected update")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn state_refresh_stops_dispatching_data_sources_after_cancellation() {
        let provider_config = carina_core::parser::ProviderConfig {
            name: "test".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(provider_config);
        parsed
            .data_sources
            .push(DataSource::with_provider("test", "lookup", "first", None));
        parsed
            .data_sources
            .push(DataSource::with_provider("test", "lookup", "second", None));
        let backend = non_empty_refresh_backend("test");
        let root = ShutdownToken::running();
        let (trigger, cancel) = root.with_graceful_shutdown_trigger();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(CancellingDataSourceRefreshFactory {
                reads: reads.clone(),
                trigger,
            })],
            &provider_configs,
        );
        let temp = tempfile::tempdir().unwrap();

        let error = run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            temp.path(),
            &ProviderContext::default(),
            cancel,
        )
        .await
        .expect_err("graceful cancellation must interrupt state refresh");

        assert!(matches!(error, AppError::Interrupted), "{error:?}");
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no data-source read may be dispatched after cancellation"
        );
    }

    #[tokio::test]
    async fn state_refresh_ignores_module_constraint_unrelated_to_data_source() {
        let composition = LiveComposition {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", "checked",
            )),
            signature: LiveCompositionSignature {
                arguments: IndexMap::from([(
                    "value".to_string(),
                    saved_composition_argument(Value::Concrete(ConcreteValue::String(
                        "bad".into(),
                    ))),
                )]),
                attributes: IndexMap::new(),
                pending_constraints: vec![PendingModuleConstraint {
                    id: ModuleConstraintId::argument_validation("value", 0),
                    expression: ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("value".to_string())),
                        op: CompareOp::Ne,
                        rhs: Box::new(ValidateExpr::String("bad".to_string())),
                    },
                    message: "value must not be bad".to_string(),
                }],
            },
            binding: Some("checked".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "checked_module".to_string(),
            instance: "root.checked".to_string(),
            call_path: crate::test_composition_call_path(),
            quoted_string_attrs: HashSet::new(),
        };
        let data_source = DataSource::with_provider("test", "lookup", "unrelated", None)
            .with_attribute(
                "query",
                Value::Concrete(ConcreteValue::String("literal".into())),
            );
        let provider_config = carina_core::parser::ProviderConfig {
            name: "test".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(provider_config);
        parsed.compositions.push(composition);
        parsed.data_sources.push(data_source);

        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("orphan", "old", "test", "old-id")
                    .expect("fixture identifier is non-empty"),
            )
            .unwrap();
        let backend = ModuleConstraintRefreshBackend { state };
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(ModuleConstraintRefreshFactory {
                reads: reads.clone(),
            })],
            &provider_configs,
        );
        let temp = tempfile::tempdir().unwrap();

        run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            temp.path(),
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("an unrelated module constraint must not block a data-source read");

        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn state_refresh_ignores_module_constraint_when_there_are_no_data_sources() {
        let composition = LiveComposition {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", "checked",
            )),
            signature: LiveCompositionSignature {
                arguments: IndexMap::from([(
                    "value".to_string(),
                    saved_composition_argument(Value::Concrete(ConcreteValue::String(
                        "bad".into(),
                    ))),
                )]),
                attributes: IndexMap::new(),
                pending_constraints: vec![PendingModuleConstraint {
                    id: ModuleConstraintId::argument_validation("value", 0),
                    expression: ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("value".to_string())),
                        op: CompareOp::Ne,
                        rhs: Box::new(ValidateExpr::String("bad".to_string())),
                    },
                    message: "value must not be bad".to_string(),
                }],
            },
            binding: Some("checked".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "checked_module".to_string(),
            instance: "root.checked".to_string(),
            call_path: crate::test_composition_call_path(),
            quoted_string_attrs: HashSet::new(),
        };
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(carina_core::parser::ProviderConfig {
            name: "test".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        });
        parsed.compositions.push(composition);

        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("orphan", "old", "test", "old-id")
                    .expect("fixture identifier is non-empty"),
            )
            .unwrap();
        let backend = ModuleConstraintRefreshBackend { state };
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_configs = parsed.providers.clone();
        let validated_config = ValidatedConfig::from_factories_for_test(
            parsed,
            vec![Box::new(ModuleConstraintRefreshFactory { reads })],
            &provider_configs,
        );
        let temp = tempfile::tempdir().unwrap();

        run_state_refresh_locked(
            validated_config,
            &backend,
            StateLock::disabled_for_test().as_ref(),
            temp.path(),
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("an unrelated module constraint cannot block an absent data-source phase");
    }

    #[test]
    fn map_lock_error_includes_non_default_project_dir_in_force_unlock_hint() {
        let error = map_lock_error(
            BackendError::Locked {
                who: "another process".to_string(),
                lock_id: "lock-123".to_string(),
                operation: "apply".to_string(),
            },
            Path::new("infra/foo"),
        );

        assert_eq!(
            error.to_string(),
            "State is locked by another process (lock ID: lock-123, operation: apply)\n\
             If you believe this is stale, run: `carina force-unlock lock-123 infra/foo`"
        );
    }

    fn outcome_unknown_state_fixture() -> StateFile {
        carina_state::check_and_migrate(
            &serde_json::json!({
                "version": StateFile::CURRENT_VERSION,
                "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
                "serial": 4,
                "lineage": "state-forget-test",
                "carina_version": "test",
                "resources": [{
                    "resource_type": "test.Resource",
                    "identity": "unknown",
                    "provider": "mock",
                    "current": {
                        "kind": "create_outcome_unknown",
                        "started_at": "2026-10-30T03:00:00Z"
                    }
                }],
                "exports": {}
            })
            .to_string(),
        )
        .expect("outcome-unknown fixture must decode")
        .into_state()
    }

    fn outcome_unknown_state_with_deposed_fixture() -> StateFile {
        carina_state::check_and_migrate(
            &serde_json::json!({
                "version": StateFile::CURRENT_VERSION,
                "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
                "serial": 4,
                "lineage": "state-forget-deposed-test",
                "carina_version": "test",
                "resources": [{
                    "resource_type": "test.Resource",
                    "identity": "unknown",
                    "provider": "mock",
                    "current": {
                        "kind": "create_outcome_unknown",
                        "started_at": "2026-10-30T03:00:00Z"
                    },
                    "deposed": [{
                        "key": "00000000-0000-4000-8000-000000000001",
                        "identifier": "old-id",
                        "attributes": {},
                        "dependency_bindings": []
                    }]
                }],
                "exports": {}
            })
            .to_string(),
        )
        .expect("outcome-unknown fixture with deposed generation must decode")
        .into_state()
    }

    #[test]
    fn state_forget_prompt_lists_deposed_generations_that_remain_tracked() {
        let mut state = outcome_unknown_state_with_deposed_fixture();
        let address = StateBlockAddress::new("mock", "test.Resource", "unknown");
        let row = state
            .find_outcome_unknown(&address)
            .expect("the fixture must select its outcome-unknown row");

        let prompt = format_state_forget_target(&address, row.resource());

        assert!(prompt.contains("Deposed generations that will remain tracked"));
        assert!(prompt.contains("identifier `old-id`"));
        assert!(prompt.contains("generation `00000000-0000-4000-8000-000000000001`"));
    }

    fn state_forget_fixture(state: &StateFile) -> CancellationFixtureBase {
        let fixture = CancellationFixtureBase::new();
        fixture.write_crn(format!(
            "backend local {{ path = \"{}\" }}\n",
            fixture.state_path().display()
        ));
        fixture.write_backend_lock();
        fixture.write_state(state);
        fixture
    }

    #[tokio::test]
    async fn state_forget_removes_and_persists_only_outcome_unknown_row() {
        let fixture = state_forget_fixture(&outcome_unknown_state_fixture());

        run_state_forget(
            "mock.test.Resource",
            "unknown",
            true,
            fixture.config_path(),
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("outcome-unknown row may be forgotten");

        let persisted = fixture.read_state().await;
        assert!(persisted.resources().is_empty());
        assert_eq!(persisted.serial, 5);
    }

    #[tokio::test]
    async fn state_forget_keeps_deposed_generation_and_next_plan_deletes_it() {
        let fixture = state_forget_fixture(&outcome_unknown_state_with_deposed_fixture());

        run_state_forget(
            "mock.test.Resource",
            "unknown",
            true,
            fixture.config_path(),
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect("the outcome-unknown current instance may be forgotten");

        let persisted = fixture.read_state().await;
        let row = persisted
            .resources()
            .first()
            .expect("the deposed-only shell must remain persisted");
        assert!(matches!(row.current(), carina_state::CurrentInstance::None));
        assert_eq!(row.deposed.len(), 1);

        let mut plan = Plan::new();
        crate::wiring::add_deposed_delete_effects(&mut plan, &Some(persisted), &[]);
        assert!(matches!(
            plan.effects(),
            [Effect::Delete {
                identifier,
                generation: carina_core::effect::EffectGeneration::Deposed(_),
                ..
            }] if identifier.as_str() == "old-id"
        ));
    }

    #[tokio::test]
    async fn state_forget_rejects_normal_row_with_typed_removed_guidance() {
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("test.Resource", "normal", "mock", "provider-id")
                    .expect("fixture identifier is non-empty"),
            )
            .expect("normal fixture row is valid");
        let fixture = state_forget_fixture(&state);

        let error = run_state_forget(
            "mock.test.Resource",
            "normal",
            true,
            fixture.config_path(),
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect_err("ordinary rows cannot be forgotten");

        assert!(matches!(&error, AppError::StateForget(_)));
        assert!(
            error
                .to_string()
                .contains("removed { from = mock.test.Resource 'normal' }")
        );
        assert_eq!(fixture.read_state().await.resources().len(), 1);
    }

    #[tokio::test]
    async fn state_forget_reports_lock_contention_like_other_locked_commands() {
        let fixture = state_forget_fixture(&outcome_unknown_state_fixture());
        let backend = fixture.backend_handle();
        let held = backend
            .acquire_lock("apply")
            .await
            .expect("hold fixture lock");

        let error = run_state_forget(
            "mock.test.Resource",
            "unknown",
            true,
            fixture.config_path(),
            &ProviderContext::default(),
            ShutdownToken::running(),
        )
        .await
        .expect_err("held lock must reject state forget");

        assert!(matches!(error, AppError::Config(_)));
        assert!(error.to_string().contains("State is locked by"));
        assert!(error.to_string().contains("operation: apply"));
        assert_eq!(fixture.read_state().await.resources().len(), 1);
        backend
            .release_lock(&held)
            .await
            .expect("release fixture lock");
    }

    #[tokio::test]
    async fn state_forget_confirmation_requires_yes_and_auto_approve_skips_input() {
        let cancelled = confirm_state_forget(
            tokio::io::BufReader::new("no\n".as_bytes()),
            ShutdownToken::running(),
            false,
        )
        .await
        .expect("confirmation input is readable");
        assert_eq!(cancelled, StateForgetConfirmation::Cancelled);

        let confirmed = confirm_state_forget(
            tokio::io::BufReader::new(tokio::io::empty()),
            ShutdownToken::running(),
            true,
        )
        .await
        .expect("auto approve does not read input");
        assert_eq!(confirmed, StateForgetConfirmation::Confirmed);
    }

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
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
            let key = (id.to_string(), identifier.as_str().to_string());
            let result = self
                .read_results
                .get(&key)
                .cloned()
                .unwrap_or_else(|| panic!("missing deposed refresh result for {:?}", key));
            Box::pin(async move {
                result
                    .map(crate::fixture_read_outcome)
                    .map_err(ProviderError::internal)
            })
        }

        fn read_data_source(
            &self,
            _resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("unexpected read_data_source")) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
        > {
            Box::pin(
                async move { Err(attempt.untracked(ProviderError::internal("unexpected create"))) },
            )
        }

        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            Box::pin(async { panic!("unexpected resume_create") })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unexpected update")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    impl ProviderNormalizer for DeposedRefreshTestProvider {
        fn normalize_desired<'a>(
            &'a self,
            _resources: &'a mut [carina_core::resource::ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut carina_core::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn hydrate_read_state<'a>(
            &'a self,
            current_states: &'a mut carina_core::provider::StateTransformMap,
            saved_attrs: &'a carina_core::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                if !self.hydrate_saved_attrs {
                    return Ok(());
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
                Ok(())
            })
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
        DeposedInstance::from_persisted_json(
            key,
            identifier.to_string(),
            provider_instance.map(str::to_string),
            attributes,
            BTreeSet::new(),
        )
    }

    fn string_value(raw: &str) -> Value {
        Value::Concrete(ConcreteValue::String(raw.to_string().into()))
    }

    fn encoded_persisted_attributes(attributes: &PersistedAttributes) -> serde_json::Value {
        serde_json::to_value(attributes).expect("persisted attributes should serialize")
    }

    #[tokio::test]
    async fn state_refresh_loads_providers_once() {
        let user_dirs = carina_core::TestUserDirs::new();
        let fixture = CancellationFixtureBase::new();
        let state_path = fixture.state_path().display();
        fixture.write_crn(format!(
            "backend local {{ path = \"{state_path}\" }}\nprovider mock {{}}\n"
        ));
        fixture.write_backend_lock();

        let load_count = crate::wiring::track_provider_loads_for_test();
        run_state_refresh(
            &user_dirs,
            fixture.config_path(),
            false,
            &ProviderContext::default(),
            fixture.shutdown_token(),
        )
        .await
        .expect("empty state refresh must succeed");

        assert_eq!(
            load_count.count(),
            1,
            "validation and locked refresh must share one provider load"
        );
    }

    #[tokio::test]
    async fn state_refresh_cleanup_priority_abandons_in_flight_read_and_releases_lock() {
        let user_dirs = carina_core::TestUserDirs::new();
        let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
        let fixture = CancellationFixtureBase::new();
        let state_path = fixture.state_path().display();
        fixture.write_crn(format!(
            "backend local {{ path = \"{state_path}\" }}\n\
             provider mock {{}}\n\
             let managed = mock.test.resource {{ name = \"managed\" }}\n"
        ));
        fixture.write_backend_lock();

        let mut initial_state = StateFile::new();
        let mut managed = ResourceState::try_applied("test.resource", "managed", "mock", "mock-id")
            .expect("fixture identifier is non-empty");
        managed.binding = Some("managed".to_string());
        managed.set_persisted_attribute("name".to_string(), serde_json::json!("managed"));
        initial_state
            .upsert_resource(managed)
            .expect("test state setup must be valid");
        let initial_serial = initial_state.serial;
        fixture.write_state(&initial_state);

        let provider_state_path = fixture.readiness_path("mock-provider-state.json");
        let provider_state = std::collections::HashMap::from([(
            "test.resource.managed".to_string(),
            serde_json::json!({ "name": "managed" }),
        )]);
        std::fs::write(
            &provider_state_path,
            carina_core::utils::pretty_with_newline(&provider_state)
                .expect("serialize mock provider state"),
        )
        .expect("write mock provider state");
        let read_ready_path = fixture.readiness_path("mock-read-ready");
        let read_outcome_path = fixture.readiness_path("mock-read-outcome");
        let _provider_env = [
            ScopedEnv::set("CARINA_MOCK_STATE_FILE", &provider_state_path),
            ScopedEnv::set("CARINA_MOCK_READ_DELAY_MS_FOR", "test.resource.managed"),
            ScopedEnv::set("CARINA_MOCK_READ_DELAY_MS", "60000"),
            ScopedEnv::set("CARINA_MOCK_READ_READY_PATH", &read_ready_path),
            ScopedEnv::set("CARINA_MOCK_READ_OUTCOME_PATH", &read_outcome_path),
        ];
        let drain_barrier = install_refresh_drain_barrier(fixture.shutdown_token());

        let provider_context = ProviderContext::default();
        let refresh = run_state_refresh(
            &user_dirs,
            fixture.config_path(),
            true,
            &provider_context,
            fixture.shutdown_token(),
        );
        tokio::pin!(refresh);
        tokio::select! {
            ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !read_ready_path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }) => ready.expect("mock provider did not signal that the blocked read started"),
            result = &mut refresh => {
                panic!("state refresh returned before the managed read blocked: {result:?}");
            }
        }
        assert!(fixture.lock_path().exists(), "fixture must hold the lock");
        fixture.shutdown_trigger().request_graceful_shutdown();
        tokio::select! {
            reached = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                drain_barrier.reached(),
            ) => reached.expect("state refresh did not enter its in-flight drain wait"),
            result = &mut refresh => {
                panic!("state refresh returned before entering its in-flight drain wait: {result:?}");
            }
        }
        fixture.shutdown_trigger().prioritize_cleanup();
        drain_barrier.release();
        let err = tokio::time::timeout(std::time::Duration::from_secs(1), refresh)
            .await
            .expect("state refresh did not abandon the in-flight read after cleanup priority")
            .unwrap_err();

        assert!(matches!(err, AppError::Interrupted));
        assert_eq!(
            std::fs::read_to_string(&read_outcome_path)
                .expect("mock provider must record the in-flight read outcome"),
            "abandoned\n",
            "the in-flight state refresh read must be dropped before producing a result"
        );
        let state = fixture.read_state().await;
        assert_eq!(
            state.serial, initial_serial,
            "abandoned refresh must not save"
        );
        assert!(
            state
                .find_resource("mock", "test.resource", "managed")
                .is_some(),
            "an abandoned refresh must preserve the persisted resource"
        );
        assert!(!fixture.lock_path().exists(), "lock must be released");
    }

    #[tokio::test]
    async fn state_refresh_expansion_hydrates_lifted_saved_enum_attrs() {
        let parsed = parse_file(
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
            std::path::Path::new("state_refresh.crn"),
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
        let current_states = HashMap::from([(
            source.id.clone(),
            Current::from_state(
                State::existing(
                    source.id.clone(),
                    HashMap::from([
                        ("name".to_string(), string_value("source")),
                        (
                            "items".to_string(),
                            Value::Concrete(ConcreteValue::List(vec![string_value("widget-1")])),
                        ),
                    ]),
                )
                .with_identifier("source-id"),
            ),
        )]);

        let source_schema = ResourceSchema::new("service.Source")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated(
                    "items",
                    AttributeType::list(AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .expect("source schema fixture must be valid");
        let mut schemas = SchemaRegistry::new();
        schemas.insert("aws", source_schema.clone());

        let preview = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted_resources,
            &current_states,
            &carina_core::data_source_value::DataSourceValues::empty(),
            &schemas,
            &carina_core::provider::RawSavedAttrs::default().lift(&schemas),
            &HashMap::new(),
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
        let mut child_state = ResourceState::try_applied(
            &child.id.resource_type,
            child.id.identity().expect("resolved identity").clone(),
            &child.id.provider,
            "widget-1",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("status", json!("Enabled"));
        child_state.directives.provider_instance = child.id.provider_instance.clone();
        state_file
            .upsert_resource(child_state)
            .expect("test state setup must be valid");

        let widget_schema = (ResourceSchema::new("service.Widget").attribute(
            AttributeSchema::optional(
                "status",
                AttributeType::enum_(
                    carina_core::schema::enum_identity("Status", Some("aws.service.Widget")),
                    Some(vec!["Enabled".to_string()]),
                    vec![("Enabled".to_string(), "enabled".to_string())],
                    None,
                    None,
                ),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid");
        schemas.insert("aws", widget_schema.clone());

        let desired_ids = sorted_resources
            .iter()
            .map(|resource| resource.id.clone())
            .collect();
        let refreshed =
            State::existing(child.id.clone(), HashMap::new()).with_identifier("widget-1");
        let provider = DeposedRefreshTestProvider::default()
            .with_saved_attr_hydration()
            .with_read_state(&child.id, "widget-1", refreshed);
        let (orphans, already_refreshed) = refresh_orphans_until_cancelled(
            &provider,
            state_file.build_orphans(&desired_ids),
            Some(&state_file),
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .expect("production orphan refresh should succeed");
        let orphans = orphans.into_plan_orphans();
        let state_file = Some(state_file);
        let expansion_prelude = crate::wiring::DeferredForExpansionPrelude::new_for_state_refresh(
            &parsed,
            &state_file,
            &schemas,
        )
        .expect("expansion prelude");
        let mut prepared_expansion = expansion_prelude
            .preserve_addresses(crate::wiring::PreserveDeferredForExpansionInputs {
                managed_current: current_states,
                orphans,
                already_refreshed,
            })
            .seal(carina_core::data_source_value::DataSourceValues::empty());
        let progress = ProgressBackend::plain(std::io::sink());
        let refresh_phase = RefreshPhase::start(&progress);
        let mut validated_config = ValidatedConfig::from_factories_with_additional_schemas_for_test(
            parsed,
            Vec::new(),
            &[],
            [
                ("aws".to_string(), source_schema),
                ("aws".to_string(), widget_schema),
            ],
        );
        let (_comparison_schemas, comparison_provider) =
            carina_core::provider::comparison_fixture_for_test(
                &schemas,
                preview.sorted_resources.iter().map(|resource| &resource.id),
            );
        validated_config
            .expand_refresh_and_lift_states(crate::wiring::ValidatedExpandRefreshAndLiftInputs {
                provider: &provider,
                comparison_provider: &comparison_provider,
                expansion: &mut prepared_expansion,
                warning_policy: crate::wiring::DeferredForWarningPolicy::Suppress,
                remote_bindings: &HashMap::new(),
                state_file: &state_file,
                saved_dep_bindings: &HashMap::new(),
                refresh_phase: &refresh_phase,
            })
            .await
            .expect("refresh expansion should succeed");
        let crate::wiring::PreparedDeferredForExpansionParts { addressed, .. } =
            prepared_expansion.into_parts();
        let crate::wiring::AddressedStateParts {
            managed_current: current_states,
            orphans,
            ..
        } = addressed.into_parts();
        assert!(
            !current_states.contains_key(&child.id),
            "expansion must not duplicate an orphan-owned child into current states"
        );
        assert!(orphans.contains_key(&child.id));

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
        let mut row = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-new")
            .expect("fixture identifier is non-empty")
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
        assert_eq!(found.identity.as_str(), "my-vpc");
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

    #[test]
    fn state_list_distinguishes_malformed_current_instance_from_deposed_shell() {
        let mut state = StateFile::new();
        let malformed = ResourceState::new("ec2.Subnet", "malformed", "awscc")
            .with_attribute("subnet_id", json!("subnet-live"));
        state
            .upsert_resource(malformed)
            .expect("test state setup must be valid");

        assert_eq!(
            format_state_list(&state),
            vec!["awscc.ec2.Subnet malformed  (missing provider identifier)"]
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
    fn lookup_json_resource_matches_state_show_json_row_after_anonymous_resource() {
        let anonymous =
            ResourceState::try_applied("test.resource", "anonymous-first", "mock", "anonymous-id")
                .expect("fixture identifier must be non-empty")
                .with_attribute("name", json!("anonymous"));

        let mut target =
            ResourceState::try_applied("test.resource", "target-resource", "mock", "target-id")
                .expect("fixture identifier must be non-empty")
                .with_attribute("name", json!("target"));
        target.binding = Some("target".to_string());

        let trailing =
            ResourceState::try_applied("test.resource", "trailing-resource", "mock", "trailing-id")
                .expect("fixture identifier must be non-empty")
                .with_attribute("name", json!("trailing"));

        let mut state = StateFile::new();
        for row in [anonymous, target, trailing] {
            state
                .upsert_resource(row)
                .expect("test state setup must be valid");
        }
        assert!(state.resources()[0].binding.is_none());

        let document =
            StateShowDocument::from_state(&state).expect("state show JSON document must render");
        let shown_target = document
            .resources
            .iter()
            .find(|resource| resource.binding.as_deref() == Some("target"))
            .expect("target row must be present in state show JSON");
        let shown_target =
            serde_json::to_value(shown_target).expect("state show resource must serialize");

        let lookup_target = format_state_lookup(&state, "target", true)
            .expect("state lookup JSON must render the target");
        let lookup_target: serde_json::Value =
            serde_json::from_str(&lookup_target).expect("state lookup must emit valid JSON");

        assert_eq!(lookup_target, shown_target);
    }

    #[test]
    fn lookup_json_in_flight_resource_returns_public_row_and_rejects_attributes() {
        let id =
            ResourceId::with_provider_identity("mock", "test.resource", "pending-resource", None);
        let resource = ResolvedResource::new(Resource::from_id(id.clone()));
        let pending = crate::fixture_in_flight_create(
            &id,
            "resume-token",
            Some("pending-id"),
            Sent::AllRequestAttributes,
        );
        let mut row = ResourceState::from_in_flight(
            &resource,
            &pending,
            None,
            None,
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        .expect("in-flight fixture must be encodable");
        row.binding = Some("pending".to_string());
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("in-flight fixture must be valid");

        let output = format_state_lookup(&state, "pending", true).unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["binding"], "pending");
        assert_eq!(output["current"]["kind"], "create_in_flight");
        assert_eq!(output["current"]["identifier"], "pending-id");
        assert_eq!(output["current"]["attributes"], json!({}));

        let error = format_state_lookup(&state, "pending.name", true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "Cannot look up attribute 'name' on resource 'pending': create in flight since"
            ),
            "unexpected error: {error}"
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
            "# awscc.ec2.Vpc (main)\n  cidr_block = '10.0.0.0/16'\n  vpc_id = 'vpc-new'\n  (deposed dep-a vpc-old)\n    cidr_block = '10.0.1.0/16'\n    vpc_id = 'vpc-old'\n  (deposed dep-b vpc-older)\n    vpc_id = 'vpc-older'\n\n# awscc.ec2.Subnet (abandoned)\n  (deposed dep-shell subnet-old)\n    subnet_id = 'subnet-old'\n"
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
        let mut row = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-current")
            .expect("fixture identifier is non-empty")
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

        let gone_id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
        let alive_id = ResourceId::with_provider_identity(
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
            &[resolved(desired)],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let row = state.find_resource("awscc", "ec2.Vpc", "main").unwrap();
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-current")
        );
        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("vpc_id"),
            carina_core::value::json_to_dsl_value(&json!("vpc-current"))
        );
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].key, alive_key);
        assert_eq!(row.deposed[0].identifier, "vpc-alive");
        assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
        assert_eq!(
            row.deposed[0].persisted_attributes().get("vpc_id"),
            carina_core::value::json_to_dsl_value(&json!("vpc-alive"))
        );
        assert_eq!(
            row.deposed[0].persisted_attributes().get("cidr_block"),
            carina_core::value::json_to_dsl_value(&json!("10.0.1.0/16"))
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

        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
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
    async fn refresh_deposed_generation_carries_previous_write_only_value_when_read_omits_it() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("carry-key"),
            "db-old",
            None,
            HashMap::from([
                ("password".to_string(), json!("previous-password")),
                ("endpoint".to_string(), json!("old.example")),
            ]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "db-old",
            State::existing(
                id.clone(),
                HashMap::from([("endpoint".to_string(), string_value("new.example"))]),
            )
            .with_identifier("db-old"),
        );
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None)
            .with_attribute("password", string_value("new-desired-password"));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("db.Instance")
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                )
                .attribute(AttributeSchema::optional("endpoint", AttributeType::string()).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[resolved(desired)],
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let attributes = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        assert_eq!(
            attributes.get("password"),
            carina_core::value::json_to_dsl_value(&json!("previous-password"))
        );
        assert_eq!(
            attributes.get("endpoint"),
            carina_core::value::json_to_dsl_value(&json!("new.example"))
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_ignores_provider_echo_for_write_only_value() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(deposed_instance(
            deposed_key("carry-key"),
            "db-old",
            None,
            HashMap::from([("password".to_string(), json!("previous-password"))]),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &id,
            "db-old",
            State::existing(
                id.clone(),
                HashMap::from([(
                    "password".to_string(),
                    string_value("provider-echo-password"),
                )]),
            )
            .with_identifier("db-old"),
        );
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None)
            .with_attribute("password", string_value("new-desired-password"));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("db.Instance").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            &[resolved(desired)],
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let attributes = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        assert_eq!(
            attributes.get("password"),
            carina_core::value::json_to_dsl_value(&json!("previous-password"))
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

        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
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
            &[resolved(desired)],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let persisted = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes()
            .clone();
        let encoded = encoded_persisted_attributes(&persisted);
        let stored =
            structural_secret_digest(encoded.get("password").expect("password should be stored"));
        assert!(stored.starts_with("$argon2id$"), "got {stored}");
        assert!(!stored.contains("plain-secret"));
    }

    #[tokio::test]
    async fn refresh_deposed_generation_carries_schema_write_only_plaintext_without_desired_secret()
    {
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

        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &ResolvedResourceId::new(id.clone()),
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
            (ResourceSchema::new("db.Instance")
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                )
                .attribute(AttributeSchema::optional("endpoint", AttributeType::string()).build()))
            .validate()
            .expect("schema fixture must be valid"),
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

        let attributes = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        assert_eq!(
            attributes.get("endpoint"),
            carina_core::value::json_to_dsl_value(&json!("db.example"))
        );
        assert_eq!(
            attributes.get("password"),
            carina_core::value::json_to_dsl_value(&json!("old-hash")),
            "deposed refresh must carry its persisted value instead of a provider echo"
        );
    }

    #[tokio::test]
    async fn refresh_deposed_generation_rehashes_existing_hash_without_desired_secret() {
        let mut state = StateFile::new();
        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let mut row = ResourceState::new("db.Instance", "main", "awscc");
        row.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key("secret-key"),
            "db-old".to_string(),
            None,
            HashMap::from([(
                "password".to_string(),
                structural_legacy_secret(&"ab".repeat(32)),
            )]),
            BTreeSet::new(),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &ResolvedResourceId::new(id.clone()),
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

        let persisted = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        let encoded = encoded_persisted_attributes(persisted);
        let stored = structural_secret_digest(
            encoded
                .get("password")
                .expect("password should remain a stored secret hash"),
        );
        assert!(stored.starts_with("$argon2id$"), "got {stored}");
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
        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
        let previous_tags = json!({
            "Name": "old-name",
            "SecretTag": structural_legacy_secret(&"ab".repeat(32)),
        });
        let mut row = ResourceState::new("ec2.Vpc", "main", "awscc");
        row.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key("nested-secret-key"),
            "vpc-old".to_string(),
            None,
            HashMap::from([("tags".to_string(), previous_tags.clone())]),
            BTreeSet::new(),
        ));
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let mut provider_tags = indexmap::IndexMap::new();
        provider_tags.insert("Name".to_string(), string_value("new-name"));
        provider_tags.insert("SecretTag".to_string(), string_value("plain-secret"));
        let provider = DeposedRefreshTestProvider::default().with_read_state(
            &ResolvedResourceId::new(id.clone()),
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

        let persisted = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        let encoded = encoded_persisted_attributes(persisted);
        let stored_tags = encoded.get("tags").expect("tags should be stored");
        assert_eq!(stored_tags["Name"], json!("new-name"));
        let secret = structural_secret_digest(&stored_tags["SecretTag"]);
        assert!(secret.starts_with("$argon2id$"), "got {secret}");
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
        let persisted_again = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        let encoded_again = encoded_persisted_attributes(persisted_again);
        let stored_again = encoded_again
            .get("tags")
            .expect("tags should still be stored");
        assert_eq!(stored_again, stored_tags);
    }

    #[test]
    fn current_orphan_refresh_preserves_all_host_metadata_and_prevent_destroy() {
        let id = ResourceId::with_provider_identity(
            "awscc",
            "ec2.Subnet",
            "main",
            Some("west".to_string()),
        );
        let mut row = ResourceState::try_applied("ec2.Subnet", "main", "awscc", "subnet-current")
            .expect("fixture identifier is non-empty")
            .with_attribute("token", json!("old-token"));
        row.directives = carina_core::resource::Directives {
            force_delete: true,
            create_before_destroy: true,
            prevent_destroy: true,
            depends_on: vec!["security_group".to_string()],
            provider_instance: Some("west".to_string()),
        };
        row.prefixes = HashMap::from([("name".to_string(), "prod-".to_string())]);
        row.protected = true;
        row.name_overrides.insert(
            "name".to_string(),
            carina_state::NameOverride {
                temp_value: "prod-replacement".to_string(),
                original_value: Some("prod-original".to_string()),
            },
        );
        row.binding = Some("subnet".to_string());
        row.dependency_bindings = BTreeSet::from(["vpc".to_string()]);
        row.explicit = carina_core::explicit::ExplicitFields::Struct {
            children: HashMap::from([(
                "cidr_block".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            )]),
        };
        row = crate::fixture_resource_state_metadata(row, Some(vec!["token".to_string()]), None);
        let deposed_key = deposed_key("prior-generation");
        row.deposed.push(deposed_instance(
            deposed_key.clone(),
            "subnet-prior",
            Some("west"),
            HashMap::from([("cidr_block".to_string(), json!("10.0.0.0/24"))]),
        ));
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let fresh = State::existing(
            id.clone(),
            HashMap::from([
                ("cidr_block".to_string(), string_value("10.0.1.0/24")),
                ("token".to_string(), string_value("provider-token")),
            ]),
        )
        .with_identifier("subnet-current");
        let mut counts = StateRefreshCounts::default();
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("ec2.Subnet").attribute(
                AttributeSchema::optional("token", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            None,
            &schemas,
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let refreshed = state
            .find_resource("awscc", "ec2.Subnet", "main")
            .expect("refreshed orphan row must remain");
        assert_eq!(refreshed.binding.as_deref(), Some("subnet"));
        assert_eq!(
            refreshed.dependency_bindings,
            BTreeSet::from(["vpc".to_string()])
        );
        assert_eq!(
            refreshed.directives,
            carina_core::resource::Directives {
                force_delete: true,
                create_before_destroy: true,
                prevent_destroy: true,
                depends_on: vec!["security_group".to_string()],
                provider_instance: Some("west".to_string()),
            }
        );
        assert_eq!(
            refreshed.prefixes,
            HashMap::from([("name".to_string(), "prod-".to_string())])
        );
        assert!(matches!(
            &refreshed.explicit,
            carina_core::explicit::ExplicitFields::Struct { children }
                if children.contains_key("cidr_block")
        ));
        assert_eq!(refreshed.write_only_attributes(), ["token"]);
        assert_eq!(
            refreshed
                .persisted_attributes()
                .expect("refreshed orphan has attributes")
                .get("token"),
            carina_core::value::json_to_dsl_value(&json!("old-token")),
            "orphan refresh must carry the previous write-only value even if the provider echoes it"
        );
        assert!(refreshed.protected);
        assert_eq!(
            refreshed.name_overrides.get("name"),
            Some(&carina_state::NameOverride {
                temp_value: "prod-replacement".to_string(),
                original_value: Some("prod-original".to_string()),
            })
        );
        assert_eq!(refreshed.deposed.len(), 1);
        assert_eq!(refreshed.deposed[0].key, deposed_key);
        assert_eq!(refreshed.deposed[0].identifier, "subnet-prior");

        let desired_ids = HashSet::new();
        let orphans = state.build_orphans(&desired_ids).into_plan_orphans();
        let plan = carina_core::differ::create_plan_for_test(
            &[],
            &[],
            &carina_core::provider::ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(&HashMap::new(), &orphans),
            &state.build_directives(),
            &schemas,
            &RawSavedAttrs::default().lift(&schemas),
            &HashMap::new(),
            &[],
        )
        .expect("orphan plan should build");
        assert!(plan.effects().is_empty());
        assert!(matches!(
            &plan.errors()[0].kind,
            carina_core::plan::PlanErrorKind::PreventDestroy {
                action: carina_core::plan::PreventDestroyAction::DeleteRemovedFromConfiguration
            }
        ));
    }

    fn refresh_reordered_list(list_type: AttributeType) -> StateRefreshCounts {
        let id = ResourceId::with_provider_identity("test", "example.Lists", "main", None);
        let desired = Resource::with_provider("test", "example.Lists", "main", None);
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("example.Lists", "main", "test", "lists-current")
                    .expect("fixture identifier is non-empty")
                    .with_attribute("items", json!(["a", "b"])),
            )
            .expect("test state setup must be valid");
        let fresh = State::existing(
            id.clone(),
            HashMap::from([(
                "items".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    string_value("b"),
                    string_value("a"),
                ])),
            )]),
        )
        .with_identifier("lists-current");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (ResourceSchema::new("example.Lists")
                .attribute(AttributeSchema::optional("items", list_type).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &ResolvedResourceId::new(id),
            &Current::from_state(fresh),
            &mut state,
            Some(&resolved(desired)),
            &schemas,
            "",
            &mut counts,
        )
        .expect("state refresh should compare the reordered list");

        counts
    }

    #[test]
    fn current_refresh_seeds_unrecorded_from_empty_desired_without_promoting_tags() {
        use carina_core::differ::{Diff, diff};
        use carina_core::explicit::ExplicitFields;

        let id = ResourceId::with_provider_identity("mock", "ec2.InternetGateway", "igw", None);
        let desired = resolved(Resource::with_provider(
            "mock",
            "ec2.InternetGateway",
            "igw",
            None,
        ));
        let mut prior = ResourceState::try_applied("ec2.InternetGateway", "igw", "mock", "igw-id")
            .expect("fixture identifier is non-empty");
        prior.explicit = ExplicitFields::Unrecorded;
        let mut state = StateFile::new();
        state
            .upsert_resource(prior)
            .expect("test state setup must be valid");

        let fresh = State::existing(
            id.clone(),
            HashMap::from([(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            )]),
        )
        .with_identifier("igw-id");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("ec2.InternetGateway").attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            Some(&desired),
            &schemas,
            "",
            &mut counts,
        )
        .expect("state refresh should write the canonical provider state");

        let row = state
            .find_resource_for_id(&id)
            .expect("refreshed IGW row must remain");
        assert_eq!(
            row.explicit,
            ExplicitFields::Struct {
                children: HashMap::new()
            },
            "refresh must repair from empty desired without promoting provider attributes"
        );
        let saved = state.build_saved_attrs().lift(&schemas);
        assert!(matches!(
            diff(
                &desired,
                &fresh,
                &saved,
                Some(&row.explicit),
                schemas
                    .get_for(&desired)
                    .map(|schema| schema.schema().as_schema()),
                &carina_core::differ::exact_comparison_context_for_tests(),
            ),
            Diff::NoChange(_)
        ));
    }

    #[test]
    fn current_refresh_keeps_prior_authored_tags_when_desired_has_removed_them() {
        use carina_core::differ::{Diff, diff};
        use carina_core::explicit::ExplicitFields;

        let id = ResourceId::with_provider_identity("mock", "ec2.InternetGateway", "igw", None);
        let desired = resolved(
            Resource::with_provider("mock", "ec2.InternetGateway", "igw", None)
                .with_attribute("name", Value::Concrete(ConcreteValue::String("x".into()))),
        );
        let mut prior = ResourceState::try_applied("ec2.InternetGateway", "igw", "mock", "igw-id")
            .expect("fixture identifier is non-empty")
            .with_attribute("name", json!("x"))
            .with_attribute("tags", json!({ "environment": "production" }));
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([
                ("name".to_string(), ExplicitFields::Leaf),
                (
                    "tags".to_string(),
                    ExplicitFields::Struct {
                        children: HashMap::from([(
                            "environment".to_string(),
                            ExplicitFields::Leaf,
                        )]),
                    },
                ),
            ]),
        };
        prior.explicit = prior_explicit.clone();
        let mut state = StateFile::new();
        state
            .upsert_resource(prior)
            .expect("test state setup must be valid");

        let fresh = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("x".into())),
                ),
                (
                    "tags".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                ),
            ]),
        )
        .with_identifier("igw-id");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("ec2.InternetGateway")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                        .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            Some(&desired),
            &schemas,
            "",
            &mut counts,
        )
        .expect("state refresh should rebuild desired authoring and retain live pending removals");

        let row = state
            .find_resource_for_id(&id)
            .expect("refreshed IGW row must remain");
        assert_eq!(
            row.explicit, prior_explicit,
            "refresh must not record an unapplied removal as authored state"
        );
        let saved = state.build_saved_attrs().lift(&schemas);
        let Diff::Update {
            changed_attributes, ..
        } = diff(
            &desired,
            &fresh,
            &saved,
            Some(&row.explicit),
            schemas
                .get_for(&desired)
                .map(|schema| schema.schema().as_schema()),
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        else {
            panic!("refresh must preserve the pending tags removal");
        };
        assert_eq!(changed_attributes, vec!["tags".to_string()]);
    }

    fn diff_after_failed_update_and_state_refresh(
        applied: Resource,
        desired: Resource,
        provider_attributes: HashMap<String, Value>,
        changed_attribute: &str,
    ) -> (
        carina_core::differ::Diff,
        carina_core::explicit::ExplicitFields,
    ) {
        use carina_core::differ::{Diff, diff};
        use carina_core::executor::AcceptedRequest;

        let id = desired.id.clone();
        let applied = resolved(applied);
        let desired = resolved(desired);
        let provider_state =
            State::existing(id.clone(), provider_attributes).with_identifier("provider-id");
        let accepted_create = AcceptedRequest::create_for_test();
        let prior = ResourceState::from_provider_state_for_resource_and_schema(
            &applied,
            &provider_state,
            None,
            None,
            RowBasis::Applied(&accepted_create),
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        .expect("accepted create should establish the original authoring tree");
        let mut prior_state = StateFile::new();
        prior_state
            .upsert_resource(prior.clone())
            .expect("prior row should be valid");
        let prior_saved = prior_state.build_saved_attrs().lift(&SchemaRegistry::new());
        let Diff::Update {
            changed_attributes, ..
        } = diff(
            &desired,
            &provider_state,
            &prior_saved,
            Some(&prior.explicit),
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        else {
            panic!("the Applied row must initially plan the nested removal");
        };
        assert_eq!(changed_attributes, vec![changed_attribute.to_string()]);

        // A failed update has no Applied result, so apply writeback carries the
        // fresh pre-update provider state against the new desired resource.
        let failed_writeback = ResourceState::from_provider_state_for_resource_and_schema(
            &desired,
            &provider_state,
            Some(&prior),
            None,
            RowBasis::Carried,
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        .expect("failed update should carry provider state");
        let mut refreshed_state = StateFile::new();
        refreshed_state
            .upsert_resource(failed_writeback)
            .expect("failed-update row should be valid");

        // Exercise the state-refresh write path between the failed update and
        // the next plan. The provider still reports the pre-update value.
        let mut counts = StateRefreshCounts::default();
        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(provider_state.clone()),
            &mut refreshed_state,
            Some(&desired),
            &SchemaRegistry::new(),
            "",
            &mut counts,
        )
        .expect("state refresh should write the fresh provider state");

        let refreshed = refreshed_state
            .find_resource_for_id(&id)
            .expect("refreshed row should remain");
        let explicit = refreshed.explicit.clone();
        let saved = refreshed_state
            .build_saved_attrs()
            .lift(&SchemaRegistry::new());
        (
            diff(
                &desired,
                &provider_state,
                &saved,
                Some(&explicit),
                None,
                &carina_core::differ::exact_comparison_context_for_tests(),
            ),
            explicit,
        )
    }

    #[test]
    fn state_refresh_nested_struct_pending_removal_after_failed_update_is_replanned() {
        use carina_core::differ::Diff;

        let settings = |description: Option<&str>| {
            let mut fields = IndexMap::from([(
                "enabled".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            )]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let applied_settings = settings(Some("web"));
        let applied = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("settings", applied_settings.clone());
        let desired = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("settings", settings(None));

        let (result, explicit) = diff_after_failed_update_and_state_refresh(
            applied,
            desired,
            HashMap::from([("settings".to_string(), applied_settings)]),
            "settings",
        );
        assert!(
            matches!(
                result,
                Diff::Update {
                    ref changed_attributes,
                    ..
                } if changed_attributes == &["settings".to_string()]
            ),
            "state refresh must preserve the pending nested struct removal; explicit={explicit:#?}, diff={result:#?}"
        );
    }

    #[test]
    fn state_refresh_list_element_nested_pending_removal_after_failed_update_is_replanned() {
        use carina_core::differ::Diff;

        let rule = |port, description: Option<&str>| {
            let mut fields = IndexMap::from([(
                "port".to_string(),
                Value::Concrete(ConcreteValue::Int(port)),
            )]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let applied_rules = Value::Concrete(ConcreteValue::List(vec![
            rule(80, Some("web")),
            rule(443, None),
        ]));
        let desired_rules =
            Value::Concrete(ConcreteValue::List(vec![rule(80, None), rule(443, None)]));
        let applied = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("rules", applied_rules.clone());
        let desired = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("rules", desired_rules);

        let (result, explicit) = diff_after_failed_update_and_state_refresh(
            applied,
            desired,
            HashMap::from([("rules".to_string(), applied_rules)]),
            "rules",
        );
        assert!(
            matches!(
                result,
                Diff::Update {
                    ref changed_attributes,
                    ..
                } if changed_attributes == &["rules".to_string()]
            ),
            "state refresh must preserve the pending list-element removal; explicit={explicit:#?}, diff={result:#?}"
        );
    }

    #[test]
    fn current_refresh_ignores_reordered_unordered_list() {
        let counts = refresh_reordered_list(AttributeType::unordered_list(AttributeType::string()));

        assert_eq!(counts.updated, 0);
        assert_eq!(counts.unchanged, 1);
    }

    #[test]
    fn current_refresh_reports_reordered_ordered_list() {
        let counts = refresh_reordered_list(AttributeType::list(AttributeType::string()));

        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 0);
    }

    #[test]
    fn current_refresh_ignores_reordered_unordered_string_or_list_union() {
        let counts = refresh_reordered_list(AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::unordered_list(AttributeType::string()),
        ]));

        assert_eq!(counts.updated, 0);
        assert_eq!(counts.unchanged, 1);
    }

    #[test]
    fn current_refresh_reports_reordered_ordered_string_or_list_union() {
        let counts = refresh_reordered_list(AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::list(AttributeType::string()),
        ]));

        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 0);
    }

    #[test]
    fn current_orphan_refresh_rehashes_existing_hash_without_desired_secret() {
        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("db.Instance", "main", "awscc", "db-current")
                    .expect("fixture identifier is non-empty")
                    .with_attribute("password", structural_legacy_secret(&"ab".repeat(32))),
            )
            .expect("test state setup must be valid");
        let fresh = State::existing(
            id.clone(),
            HashMap::from([("password".to_string(), string_value("plain-secret"))]),
        )
        .with_identifier("db-current");
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            None,
            &SchemaRegistry::new(),
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let persisted = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .persisted_attributes();
        let encoded = encoded_persisted_attributes(
            persisted.expect("refreshed current row has applied attributes"),
        );
        let stored = structural_secret_digest(
            encoded
                .get("password")
                .expect("password should remain a stored secret hash"),
        );
        assert!(stored.starts_with("$argon2id$"), "got {stored}");
        assert!(!stored.contains("plain-secret"));
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 0);
    }

    #[test]
    fn current_orphan_refresh_merges_nested_secret_hash_per_leaf_and_converges() {
        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
        let previous_tags = json!({
            "Name": "old-name",
            "SecretTag": structural_legacy_secret(&"ab".repeat(32)),
        });
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-current")
                    .expect("fixture identifier is non-empty")
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
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            None,
            &SchemaRegistry::new(),
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let persisted = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .persisted_attributes();
        let encoded = encoded_persisted_attributes(
            persisted.expect("refreshed current row has applied attributes"),
        );
        let stored_tags = encoded.get("tags").expect("tags should be stored");
        assert_eq!(stored_tags["Name"], json!("new-name"));
        let secret = structural_secret_digest(&stored_tags["SecretTag"]);
        assert!(secret.starts_with("$argon2id$"), "got {secret}");
        assert!(!secret.contains("plain-secret"));
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 0);

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            None,
            &SchemaRegistry::new(),
            " (orphan)",
            &mut counts,
        )
        .unwrap();

        let persisted_again = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .unwrap()
            .persisted_attributes();
        let encoded_again = encoded_persisted_attributes(
            persisted_again.expect("refreshed current row has applied attributes"),
        );
        let stored_again = encoded_again
            .get("tags")
            .expect("tags should still be stored");
        assert_eq!(stored_again, stored_tags);
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.unchanged, 1);
    }

    #[test]
    fn current_refresh_drops_schema_write_only_plaintext_absent_from_desired() {
        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None);
        let mut state = StateFile::new();
        state
            .upsert_resource(
                ResourceState::try_applied("db.Instance", "main", "awscc", "db-current")
                    .expect("fixture identifier is non-empty"),
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
            (ResourceSchema::new("db.Instance").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut counts = StateRefreshCounts::default();

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            Some(&resolved(desired.clone())),
            &schemas,
            "",
            &mut counts,
        )
        .unwrap();

        let row = state.find_resource("awscc", "db.Instance", "main").unwrap();
        assert!(
            !row.persisted_attributes()
                .expect("row has applied attributes")
                .contains_key("password"),
            "write-only plaintext must not be persisted by refresh"
        );
        assert_eq!(counts.updated, 0);
        assert_eq!(counts.unchanged, 1);
    }

    #[test]
    fn current_refresh_carries_previous_write_only_value_instead_of_desired() {
        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None)
            .with_attribute("password", string_value("new"));
        let existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-current")
            .expect("fixture identifier is non-empty")
            .with_attribute("password", json!("old"));
        let existing = crate::fixture_resource_state_metadata(
            existing,
            Some(vec!["password".to_string()]),
            None,
        );
        let mut state = StateFile::new();
        state
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let fresh = State::existing(id.clone(), HashMap::new()).with_identifier("db-current");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("db.Instance").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        diff_display_update_resource(
            &ResolvedResourceId::new(id),
            &Current::from_state(fresh),
            &mut state,
            Some(&resolved(desired)),
            &schemas,
            "",
            &mut StateRefreshCounts::default(),
        )
        .expect("state refresh should write the refreshed row");

        let row = state.find_resource("awscc", "db.Instance", "main").unwrap();
        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("password"),
            carina_core::value::json_to_dsl_value(&json!("old")),
        );
        assert_eq!(row.write_only_attributes(), ["password"]);
    }

    #[test]
    fn current_refresh_carries_plain_raw_legacy_marker_in_write_only_attribute() {
        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
        let marker = format!("{LEGACY_SECRET_PREFIX}{}", "ab".repeat(32));
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None)
            .with_attribute("password", string_value("new-value"));
        let existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-current")
            .expect("fixture identifier is non-empty")
            .with_attribute("password", json!(marker.clone()));
        let existing = crate::fixture_resource_state_metadata(
            existing,
            Some(vec!["password".to_string()]),
            None,
        );
        let mut state = StateFile::new();
        state
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let fresh = State::existing(id.clone(), HashMap::new()).with_identifier("db-current");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("db.Instance").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        diff_display_update_resource(
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            Some(&resolved(desired)),
            &schemas,
            "",
            &mut StateRefreshCounts::default(),
        )
        .expect("marker-shaped write-only plaintext must remain an ordinary string");

        let row = state
            .find_resource("awscc", "db.Instance", "main")
            .expect("refreshed row must remain");
        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("password"),
            carina_core::value::json_to_dsl_value(&json!(marker))
        );
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

        let id = ResourceId::with_provider_identity("awscc", "db.Instance", "main", None);
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

        let attrs = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        assert_eq!(
            attrs.get("endpoint"),
            carina_core::value::json_to_dsl_value(&json!("new.example"))
        );
        assert_eq!(
            attrs.get("description"),
            carina_core::value::json_to_dsl_value(&json!("kept from state"))
        );
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
            (ResourceSchema::new("service.Widget").attribute(
                AttributeSchema::optional(
                    "status",
                    AttributeType::enum_(
                        carina_core::schema::enum_identity("Status", Some("awscc.service.Widget")),
                        Some(vec!["Enabled".to_string()]),
                        vec![("Enabled".to_string(), "enabled".to_string())],
                        None,
                        None,
                    ),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
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

        let id = ResourceId::with_provider_identity("awscc", "service.Widget", "main", None);
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
            std::slice::from_ref(&resolved(desired.clone())),
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
            .persisted_attributes()
            .clone();
        let after_first_json = encoded_persisted_attributes(&after_first);
        assert!(
            after_first_json["status"].get("$enum").is_some(),
            "enum state should be stored in canonical enum shape"
        );

        let second = refresh_deposed_generations_until_cancelled(
            &provider,
            &mut state,
            std::slice::from_ref(&resolved(desired.clone())),
            &schemas,
            &ShutdownToken::running(),
        )
        .await
        .unwrap();
        let after_second = state
            .find_resource("awscc", "service.Widget", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes()
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

        let id = ResourceId::with_provider_identity(
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
            &[resolved(wrong_instance), resolved(right_instance)],
            &SchemaRegistry::new(),
            &ShutdownToken::running(),
        )
        .await
        .unwrap();

        let persisted = state
            .find_resource("awscc", "db.Instance", "main")
            .unwrap()
            .deposed[0]
            .persisted_attributes();
        let encoded = encoded_persisted_attributes(persisted);
        let stored = encoded
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

        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
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
            row.deposed[0].persisted_attributes().get("vpc_id"),
            carina_core::value::json_to_dsl_value(&json!("vpc-old"))
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
        let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main", None);
        let mut state = StateFile::new();
        let mut row = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-current")
            .expect("fixture identifier is non-empty")
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
            &ResolvedResourceId::new(id.clone()),
            &Current::from_state(fresh.clone()),
            &mut state,
            Some(&resolved(desired.clone())),
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

    // --- carina#3338: module-prefixed bindings ---

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
    fn lookup_exports_is_an_ordinary_resource_address() {
        let state = load_fixture("state_lookup_modules_exports");
        let error = format_state_lookup(&state, "exports", false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Resource 'exports' not found in state")
        );
    }

    #[test]
    fn lookup_resource_binding_named_exports() {
        let mut state = StateFile::new();
        let mut rs = ResourceState::new("ec2.Vpc", "exports-vpc", "awscc");
        rs.binding = Some("exports".to_string());
        rs.set_persisted_attribute("vpc_id".to_string(), serde_json::json!("vpc-from-resource"));
        state
            .upsert_resource(rs)
            .expect("test state setup must be valid");
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
    fn completion_does_not_offer_exports() {
        let state = load_fixture("state_lookup_modules_exports");
        assert!(complete_state_lookup_from(&state, "exports").is_empty());
    }
}
