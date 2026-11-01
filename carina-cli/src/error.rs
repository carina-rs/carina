//! Typed application error for carina-cli

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use carina_core::hint::ProjectCommand;
use carina_core::plan::PlanError;
use carina_core::provider::{ProviderError, ProviderInstanceKey, ReferenceScope};
use carina_core::resource::{ResolvedResourceId, ResourceId};
use carina_state::BackendError;

use crate::commands::shared::state_writeback::PersistedDirectiveChange;

#[derive(Debug)]
pub struct SavedPlanSourceResolutionError {
    pub source_path: carina_core::paths::DisplayPath,
    pub replan_command: String,
    pub source: std::io::Error,
}

impl std::fmt::Display for SavedPlanSourceResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to resolve saved plan source directory {}. Re-run `{}`.",
            self.source_path, self.replan_command,
        )
    }
}

impl std::error::Error for SavedPlanSourceResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Final endpoint/kind summary retaining typed causes for programmatic use.
/// Full cause chains have already been streamed through the failure sink.
#[derive(Debug, Clone)]
pub struct ExecutionFailureReport {
    summary: String,
    failures: Vec<carina_core::executor::EffectFailure>,
}

impl ExecutionFailureReport {
    pub fn new(summary: String, failures: &[carina_core::executor::EffectFailure]) -> Self {
        Self {
            summary,
            failures: failures.to_vec(),
        }
    }

    pub fn failures(&self) -> &[carina_core::executor::EffectFailure] {
        &self.failures
    }
}

impl std::fmt::Display for ExecutionFailureReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)?;
        for failure in &self.failures {
            let endpoint = failure
                .endpoint()
                .map(ToString::to_string)
                .unwrap_or_else(|| "module constraint".into());
            write!(f, "\n  {endpoint}: ")?;
            match failure.kind() {
                kind @ carina_core::executor::EffectFailureKind::BlockedByDependencyCycle {
                    ..
                } => {
                    std::fmt::Display::fmt(kind, f)?;
                }
                kind => f.write_str(kind.label())?,
            }
        }
        Ok(())
    }
}

impl std::error::Error for ExecutionFailureReport {}

/// State encoding failure annotated with the resource row being written.
#[derive(Debug)]
pub struct LocatedResourceStateEncodingError {
    resource: ResourceId,
    source: carina_state::ResourceStateEncodingError,
}

impl std::fmt::Display for LocatedResourceStateEncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to encode state for resource {}: {}",
            self.resource, self.source
        )
    }
}

impl std::error::Error for LocatedResourceStateEncodingError {}

impl LocatedResourceStateEncodingError {
    /// Resource row whose value could not be encoded.
    pub fn resource(&self) -> &ResourceId {
        &self.resource
    }

    /// Typed terminal encoding failure.
    pub fn encoding_error(&self) -> &carina_state::ResourceStateEncodingError {
        &self.source
    }
}

/// A backend error rendered with the concrete project directory available at
/// the CLI boundary.
#[derive(Debug)]
pub struct ProjectBackendError {
    project_dir: carina_core::paths::DisplayPath,
    source: BackendError,
}

impl ProjectBackendError {
    fn new(source: BackendError, project_dir: carina_core::paths::DisplayPath) -> Self {
        Self {
            project_dir,
            source,
        }
    }

    /// Typed backend failure rendered with this project's recovery command.
    pub fn backend_error(&self) -> &BackendError {
        &self.source
    }
}

impl std::fmt::Display for ProjectBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = self.source.invalid_resource_identity() {
            let plan_command =
                ProjectCommand::for_display_path("plan", &self.project_dir).to_string();
            f.write_str(&error.render_with_plan_command(&plan_command))
        } else {
            std::fmt::Display::fmt(&self.source, f)
        }
    }
}

// This is a terminal project-aware rendering surface. In particular, the
// legacy-state recovery message embeds a project-specific `carina plan`
// command, so exposing the backend error again would repeat the diagnostic.
impl std::error::Error for ProjectBackendError {}

/// Typed failure while reloading state-owned create status under the apply
/// lock. This type deliberately has no plain `Display`: the plan-error branch
/// can contain a project-scoped recovery command.
#[derive(Debug)]
pub(crate) enum LockedStateReloadError {
    Config(String),
    Plan(Box<PlanError>),
}

impl From<String> for LockedStateReloadError {
    fn from(message: String) -> Self {
        Self::Config(message)
    }
}

impl From<PlanError> for LockedStateReloadError {
    fn from(error: PlanError) -> Self {
        Self::Plan(Box::new(error))
    }
}

impl LockedStateReloadError {
    pub(crate) fn display_with_project_dir<'a>(
        &'a self,
        project_dir: &'a Path,
    ) -> impl std::fmt::Display + 'a {
        LockedStateReloadErrorDisplay {
            error: self,
            project_dir,
        }
    }
}

struct LockedStateReloadErrorDisplay<'a> {
    error: &'a LockedStateReloadError,
    project_dir: &'a Path,
}

impl std::fmt::Display for LockedStateReloadErrorDisplay<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.error {
            LockedStateReloadError::Config(message) => formatter.write_str(message),
            LockedStateReloadError::Plan(error) => {
                std::fmt::Display::fmt(&error.display_with_project_dir(self.project_dir), formatter)
            }
        }
    }
}

/// A locked-state reload failure rendered with the concrete project directory
/// available at the apply boundary.
#[derive(Debug)]
pub struct ProjectLockedStateReloadError {
    project_dir: PathBuf,
    source: LockedStateReloadError,
}

impl ProjectLockedStateReloadError {
    fn new(source: LockedStateReloadError, project_dir: &Path) -> Self {
        Self {
            project_dir: project_dir.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for ProjectLockedStateReloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(
            &self.source.display_with_project_dir(&self.project_dir),
            formatter,
        )
    }
}

// This is the terminal rendering surface: the typed source cannot be exposed
// without re-rendering a project-scoped command without its project path.
impl std::error::Error for ProjectLockedStateReloadError {}

/// A `state forget` rejection rendered with the concrete project directory
/// available at the CLI boundary.
#[derive(Debug)]
pub struct ProjectStateForgetError {
    project_dir: PathBuf,
    source: carina_state::NotOutcomeUnknown,
}

impl ProjectStateForgetError {
    fn new(source: carina_state::NotOutcomeUnknown, project_dir: &Path) -> Self {
        Self {
            project_dir: project_dir.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for ProjectStateForgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source.display_with_project_dir(&self.project_dir), f)
    }
}

// Like `ProjectBackendError`, this project-aware wrapper is the terminal
// rendering surface. Exposing its source would print the rejection twice.
impl std::error::Error for ProjectStateForgetError {}

/// All provider-schema differences that invalidate one saved plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanSchemaMismatchError {
    pub providers: BTreeMap<String, Vec<String>>,
    pub replan_command: String,
}

impl std::fmt::Display for SavedPlanSchemaMismatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Provider schema mismatch; changed or missing types:")?;
        for (provider, changed_types) in &self.providers {
            for changed_type in changed_types {
                write!(formatter, "\n  - provider `{provider}`: {changed_type}")?;
            }
        }
        write!(
            formatter,
            "\nRe-run `{}` to produce a new saved plan before applying.",
            self.replan_command
        )
    }
}

impl std::error::Error for SavedPlanSchemaMismatchError {}

/// Recorded and currently loaded artifact summaries for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanProviderArtifactDifference {
    pub recorded: Option<String>,
    pub loaded: Option<String>,
}

/// All provider-artifact differences that invalidate one saved plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanProviderArtifactMismatchError {
    pub providers: BTreeMap<String, SavedPlanProviderArtifactDifference>,
    pub replan_command: String,
}

impl std::fmt::Display for SavedPlanProviderArtifactMismatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Provider artifact mismatch:")?;
        for (provider, difference) in &self.providers {
            let recorded = difference.recorded.as_deref().unwrap_or("missing");
            let loaded = difference
                .loaded
                .as_deref()
                .unwrap_or("not loaded (see the provider load error above)");
            write!(
                formatter,
                "\n  - provider `{provider}`: recorded {recorded}; loaded {loaded}"
            )?;
        }
        write!(
            formatter,
            "\nRe-run `{}` to produce a new saved plan before applying.",
            self.replan_command
        )
    }
}

impl std::error::Error for SavedPlanProviderArtifactMismatchError {}

/// Saved and newly initialized scopes for one provider instance differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanProviderScopeMismatchError {
    pub instance: ProviderInstanceKey,
    pub recorded: ReferenceScope,
    pub initialized: Option<ReferenceScope>,
    pub replan_command: String,
}

impl std::fmt::Display for SavedPlanProviderScopeMismatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let instance = match self.instance.provider_instance() {
            Some(binding) => format!("{}.{}", self.instance.provider(), binding),
            None => format!("{} (default)", self.instance.provider()),
        };
        write!(
            formatter,
            "Provider scope mismatch for `{instance}`: recorded {}; initialized {}.\nRe-run `{}` to produce a new saved plan before applying.",
            reference_scope_summary(&self.recorded),
            self.initialized
                .as_ref()
                .map(reference_scope_summary)
                .unwrap_or_else(|| "missing".to_string()),
            self.replan_command,
        )
    }
}

fn reference_scope_summary(scope: &ReferenceScope) -> String {
    match scope {
        ReferenceScope::Available(values) => format!(
            "{}/{}/{}",
            values.partition(),
            values.region(),
            values.account()
        ),
        ReferenceScope::Unavailable => "unavailable".to_string(),
    }
}

impl std::error::Error for SavedPlanProviderScopeMismatchError {}

/// The state-locked directive diff no longer matches the exact set recorded
/// in a saved plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanDirectiveChangesMismatchError {
    pub(crate) recorded: Vec<PersistedDirectiveChange>,
    pub(crate) recomputed: Vec<PersistedDirectiveChange>,
    pub(crate) replan_command: String,
}

impl std::fmt::Display for SavedPlanDirectiveChangesMismatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "The saved plan's resource directive changes no longer match the locked state.",
        )?;
        write_directive_changes(formatter, "Recorded by the plan", &self.recorded)?;
        write_directive_changes(formatter, "Recomputed from locked state", &self.recomputed)?;
        write!(
            formatter,
            "\nRe-run `{}` to produce a new saved plan before applying.",
            self.replan_command
        )
    }
}

fn write_directive_changes(
    formatter: &mut std::fmt::Formatter<'_>,
    label: &str,
    changes: &[PersistedDirectiveChange],
) -> std::fmt::Result {
    write!(formatter, "\n{label}:")?;
    if changes.is_empty() {
        return formatter.write_str(" none");
    }
    for change in changes {
        write!(formatter, "\n  - {}", change.resource_id)?;
        for field in &change.fields {
            write!(
                formatter,
                "\n      {}: {} -> {}",
                field.name(),
                field.rendered_from(),
                field.rendered_to()
            )?;
        }
    }
    Ok(())
}

impl std::error::Error for SavedPlanDirectiveChangesMismatchError {}

/// A backend-selection failure annotated with the upstream project whose
/// state was being resolved.
#[derive(Debug)]
pub struct UpstreamStateSourceError {
    source_dir: carina_core::paths::DisplayPath,
    source: Box<AppError>,
}

impl UpstreamStateSourceError {
    fn new(source: AppError, source_dir: carina_core::paths::DisplayPath) -> Self {
        Self {
            source_dir,
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for UpstreamStateSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "upstream_state source {}: {}",
            self.source_dir, self.source
        )
    }
}

// This is a terminal contextual rendering: Display already includes the
// wrapped diagnostic, so exposing it again as Error::source would duplicate
// the message in source-chain renderers.
impl std::error::Error for UpstreamStateSourceError {}

/// Provider preparation failure annotated with the same resource header that
/// `ProviderError::for_resource` historically rendered.
#[derive(Debug)]
pub struct ResourceProviderPreparationError {
    resource: ResourceId,
    source: Box<carina_core::executor::ProviderPreparationError>,
}

impl ResourceProviderPreparationError {
    /// Resource whose provider-boundary preparation failed.
    pub fn resource(&self) -> &ResourceId {
        &self.resource
    }

    /// Typed terminal preparation diagnostic.
    pub fn preparation_error(&self) -> &carina_core::executor::ProviderPreparationError {
        &self.source
    }
}

impl std::fmt::Display for ResourceProviderPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}.{}] {}",
            self.resource.resource_type,
            self.resource.identity_display(),
            self.source,
        )
    }
}

impl std::error::Error for ResourceProviderPreparationError {}

/// Provider preparation failure while creating the managed state bucket.
#[derive(Debug)]
pub struct StateBucketPreparationError {
    source: Box<carina_core::executor::ProviderPreparationError>,
}

impl StateBucketPreparationError {
    /// Typed terminal preparation diagnostic.
    pub fn preparation_error(&self) -> &carina_core::executor::ProviderPreparationError {
        &self.source
    }
}

impl std::fmt::Display for StateBucketPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to prepare state bucket before create: {}",
            self.source
        )
    }
}

impl std::error::Error for StateBucketPreparationError {}

/// A classified state-bucket create failure, optionally accompanied by the
/// failure to persist the recovery row. Both classified failures remain
/// available as typed values; when recovery persistence also fails, that
/// later failure is the error-chain source and the create failure remains
/// available through [`StateBucketCreateError::create_failure`].
#[derive(Debug)]
pub struct StateBucketCreateError {
    create: Box<carina_core::provider::CreateFailure>,
    seed: Option<Box<AppError>>,
}

impl StateBucketCreateError {
    pub fn new(create: carina_core::provider::CreateFailure, seed: Option<AppError>) -> Self {
        Self {
            create: Box::new(create),
            seed: seed.map(Box::new),
        }
    }

    pub fn create_failure(&self) -> &carina_core::provider::CreateFailure {
        &self.create
    }

    pub fn seed_error(&self) -> Option<&AppError> {
        self.seed.as_deref()
    }
}

impl std::fmt::Display for StateBucketCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Failed to create state bucket")
    }
}

impl std::error::Error for StateBucketCreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.seed
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
            .or_else(|| Some(self.create.as_ref()))
    }
}

/// A state bucket was created successfully, but its protected recovery row
/// could not be persisted.
#[derive(Debug)]
pub struct StateBucketSeedError {
    identifier: String,
    resource_address: String,
    source: BackendError,
}

impl StateBucketSeedError {
    pub fn new(identifier: String, resource_address: String, source: BackendError) -> Self {
        Self {
            identifier,
            resource_address,
            source,
        }
    }

    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    pub fn resource_address(&self) -> &str {
        &self.resource_address
    }
}

impl std::fmt::Display for StateBucketSeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "State bucket '{}' was created but could not be recorded in state, so it exists outside state. Add the following import block to the configuration and re-run apply to record it:",
            self.identifier
        )
    }
}

impl std::error::Error for StateBucketSeedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn apply_input_expectation() -> String {
    format!(
        "`{}` expects a project directory or a plan file written by `{}`.",
        ProjectCommand::new("apply", Path::new(".")),
        ProjectCommand::new("plan --out", Path::new(".")),
    )
}

fn unreadable_json_plan_guidance() -> String {
    format!(
        "If it is a saved plan, it may be truncated or corrupted; re-create it with `{}`.",
        ProjectCommand::new("plan --out", Path::new(".")),
    )
}

fn plan_file_recreation_guidance(replan_command: Option<&str>) -> String {
    match replan_command {
        Some(command) => {
            format!("Re-run `{command}` to produce a plan in the current format.")
        }
        None => "Re-create the plan with plan --out from the project that produced it.".to_string(),
    }
}

/// A saved-plan JSON decoding failure with its structural location retained.
#[derive(Debug)]
pub struct PlanFileJsonError {
    json_path: String,
    source: serde_json::Error,
}

impl PlanFileJsonError {
    pub(crate) fn from_path_error(error: serde_path_to_error::Error<serde_json::Error>) -> Self {
        let json_path = error.path().to_string();
        Self {
            json_path,
            source: error.into_inner(),
        }
    }

    pub(crate) fn at_root(source: serde_json::Error) -> Self {
        Self {
            json_path: ".".to_string(),
            source,
        }
    }

    /// JSON location at which decoding stopped.
    pub fn json_path(&self) -> &str {
        &self.json_path
    }

    /// Serde's typed error classification for the terminal failure.
    pub fn classify(&self) -> serde_json::error::Category {
        self.source.classify()
    }

    /// Terminal serde error, retained as the source of this location wrapper.
    pub fn serde_error(&self) -> &serde_json::Error {
        &self.source
    }
}

impl std::fmt::Display for PlanFileJsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = if self.json_path == "." {
            "<root>"
        } else {
            &self.json_path
        };
        write!(f, "{} at JSON location {path}", self.source)
    }
}

impl std::error::Error for PlanFileJsonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// A malformed current-version saved plan, annotated with its file and
/// regeneration guidance while retaining the located JSON decoding error.
#[derive(Debug)]
pub struct CorruptPlanFileError {
    path: PathBuf,
    replan_command: Option<String>,
    source: PlanFileJsonError,
}

impl CorruptPlanFileError {
    fn new(path: PathBuf, replan_command: Option<String>, source: PlanFileJsonError) -> Self {
        Self {
            path,
            replan_command,
            source,
        }
    }

    /// Located typed JSON decoding failure.
    pub fn json_error(&self) -> &PlanFileJsonError {
        &self.source
    }
}

impl std::fmt::Display for CorruptPlanFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to parse plan file '{}'.\nThe plan file may be corrupted. {} This will regenerate the plan; a corrupted or hand-edited plan cannot be trusted.",
            self.path.display(),
            plan_file_recreation_guidance(self.replan_command.as_deref())
        )
    }
}

impl std::error::Error for CorruptPlanFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Render a provider initialization error as user-facing text.
///
/// Detects the carina-provider-aws / carina-provider-awscc account
/// guard message shape ("AWS account ID ... is not in ...
/// allowed_account_ids ..." or "... in ... forbidden_account_ids ...")
/// and reformats it as a structured block. Any other provider error
/// flows through unchanged so the caller can still print it as
/// `Error: {msg}` (#2407).
///
/// Returns `None` when the message does not match the account-guard
/// shape; the caller should fall back to the generic display.
pub fn format_account_guard_error(msg: &str, provider_name: Option<&str>) -> Option<String> {
    let stripped = msg.trim();

    let allowed = parse_account_guard_clause(stripped, "allowed_account_ids");
    let forbidden = parse_account_guard_clause(stripped, "forbidden_account_ids");
    let parsed = allowed.or(forbidden)?;

    let provider = provider_name.unwrap_or("aws");
    let kind_label = match parsed.kind {
        AccountGuardKind::Allowed => "allowed_account_ids",
        AccountGuardKind::Forbidden => "forbidden_account_ids",
    };
    let expected_summary = match parsed.kind {
        AccountGuardKind::Allowed => format!("{} ({})", parsed.list_summary, kind_label),
        AccountGuardKind::Forbidden => format!("not {} ({})", parsed.list_summary, kind_label),
    };

    let mut out = String::from("AWS account mismatch\n");
    out.push_str(&format!("  Provider:    {}\n", provider));
    out.push_str(&format!("  Expected:    {}\n", expected_summary));
    out.push_str(&format!("  Actual:      {}\n", parsed.actual));
    out.push_str(
        "  Action:      Refusing to operate. Check AWS_PROFILE / aws-vault / SSO session.",
    );
    Some(out)
}

#[derive(Debug, Clone, Copy)]
enum AccountGuardKind {
    Allowed,
    Forbidden,
}

#[derive(Debug)]
struct AccountGuardClause {
    kind: AccountGuardKind,
    actual: String,
    /// Comma-separated rendering of the configured list, with surrounding
    /// brackets and quotes stripped — e.g. `151116838382`.
    list_summary: String,
}

/// Look for `AWS account ID '<id>' is (not) in (the provider's)?
/// <kind> [...]` or the plain-quote variant produced by
/// carina-provider-aws ("AWS account ID 019115212452 ..."). Returns
/// the parsed pieces if `kind` matches.
fn parse_account_guard_clause(msg: &str, kind: &str) -> Option<AccountGuardClause> {
    if !msg.contains(kind) {
        return None;
    }
    let after_id = msg.strip_prefix("AWS account ID ")?;
    // Account ID may be quoted (awscc shape) or unquoted (aws shape).
    let (account, rest) = if let Some(quoted) = after_id.strip_prefix('\'') {
        let end = quoted.find('\'')?;
        (&quoted[..end], &quoted[end + 1..])
    } else {
        let end = after_id.find(' ')?;
        (&after_id[..end], &after_id[end..])
    };
    let rest = rest.trim_start();

    let guard_kind = if rest.starts_with("is not in") {
        AccountGuardKind::Allowed
    } else if rest.starts_with("is in") || rest.starts_with("is listed in") {
        AccountGuardKind::Forbidden
    } else {
        return None;
    };

    // Match the kind we were asked to detect.
    match (guard_kind, kind) {
        (AccountGuardKind::Allowed, "allowed_account_ids") => {}
        (AccountGuardKind::Forbidden, "forbidden_account_ids") => {}
        _ => return None,
    }

    // Extract `["..."]` style list. Both providers debug-format a Vec<String>.
    let bracket_start = rest.find('[')?;
    let bracket_end = rest[bracket_start..].find(']')?;
    let inner = &rest[bracket_start + 1..bracket_start + bracket_end];
    let summary = inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .collect::<Vec<_>>()
        .join(", ");

    Some(AccountGuardClause {
        kind: guard_kind,
        actual: account.to_string(),
        list_summary: summary,
    })
}

/// Typed causes for saved-plan secret verification failures.
#[derive(Debug, thiserror::Error)]
pub enum SavedPlanSecretVerificationError {
    #[error(
        "Saved-plan secret verification failed at {location}: the secret changed since the plan was created; re-run `{replan}`."
    )]
    HashMismatch { location: String, replan: String },

    #[error(
        "Saved-plan secret verification failed at {location}: the configuration changed and the recorded secret position no longer exists; re-run `{replan}`."
    )]
    ConfigurationChanged { location: String, replan: String },

    #[error(
        "Saved-plan secret verification failed because the current configuration could not be evaluated (for example, an environment variable or decryption key may be unavailable); re-run `{replan}` after restoring the required inputs: {error}"
    )]
    EvaluationUnavailable {
        replan: String,
        error: Box<AppError>,
    },

    #[error(
        "Saved-plan secret verification failed at {location}: the current secret could not be evaluated (for example, an environment variable or decryption key may be unavailable); re-run `{replan}` after restoring the required inputs."
    )]
    SecretEvaluationUnavailable { location: String, replan: String },

    #[error(
        "Saved-plan secret verification failed because orphan resource {resource} no longer exists; re-run `{replan}`."
    )]
    OrphanVanished { resource: String, replan: String },

    #[error(
        "Saved-plan secret verification failed at {location}: the current secret could not be encoded for verification: {error}; re-run `{replan}`."
    )]
    Serialization {
        location: String,
        replan: String,
        error: carina_core::value::SerializationError,
    },
}

/// Destroy dependency analysis failure with CLI-level context.
#[derive(Debug)]
pub struct DestroyDependencyAnalysisError {
    source: carina_core::effect::deps::DependencyAnalysisFailure,
}

impl DestroyDependencyAnalysisError {
    pub fn new(source: carina_core::effect::deps::DependencyAnalysisFailure) -> Self {
        Self { source }
    }
}

impl std::fmt::Display for DestroyDependencyAnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Failed to analyze destroy dependencies")
    }
}

impl std::error::Error for DestroyDependencyAnalysisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Failure to resolve the run-scoped base credentials before starting an
/// AWS-kind provider guest.
#[derive(Clone, Debug)]
pub struct AwsCredentialStartupError {
    source: carina_plugin_host::AwsCredentialError,
}

impl AwsCredentialStartupError {
    pub(crate) fn new(source: carina_plugin_host::AwsCredentialError) -> Self {
        Self { source }
    }

    /// Returns the typed host credential-source failure.
    pub fn credential_error(&self) -> &carina_plugin_host::AwsCredentialError {
        &self.source
    }
}

impl std::fmt::Display for AwsCredentialStartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Failed to resolve AWS credentials: {}",
            self.source
        )?;
        if matches!(
            self.source,
            carina_plugin_host::AwsCredentialError::NotFound(_)
        ) {
            formatter.write_str(
                "\nSet AWS_PROFILE (and run `aws sso login` for SSO), or set \
                 AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (plus AWS_SESSION_TOKEN when needed).",
            )?;
        }
        Ok(())
    }
}

// Display is the terminal rendering surface because it adds the actionable
// CLI hint. The typed source remains available through `credential_error()`.
impl std::error::Error for AwsCredentialStartupError {}

/// Typed error enum for carina-cli operations
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Provider installation, egress consent, or initialization failed.
    #[error(transparent)]
    Init(Box<crate::commands::init::InitError>),

    /// User-owned Carina directories could not be resolved safely.
    #[error(transparent)]
    UserDirs(#[from] carina_core::UserDirsError),

    /// An official provider artifact lacks usable user-owned verification.
    #[error(transparent)]
    UnverifiedOfficialProvider(#[from] carina_provider_resolver::UnverifiedOfficialProviderError),

    /// A local provider privilege grant was malformed or unusable.
    #[error(transparent)]
    TrustedLocalGrant(#[from] carina_provider_resolver::GrantParseError),

    /// A runtime string was not a valid resource identity.
    #[error(transparent)]
    ResourceIdentity(#[from] carina_core::resource::ResourceIdentityError),

    /// A composition call source could not be converted to a portable saved
    /// identity.
    #[error(transparent)]
    CompositionSourceIdentity(#[from] carina_core::resource::CompositionSourceIdentityError),

    /// A state-bucket argument was not a valid resource identity.
    #[error("State bucket name '{bucket_name}' is not a valid resource identity: {error}")]
    InvalidStateBucketIdentity {
        bucket_name: String,
        error: carina_core::resource::ResourceIdentityError,
    },

    /// A state-bucket argument was not a valid provider identifier.
    #[error("State bucket name '{bucket_name}' is not a valid provider identifier: {error}")]
    InvalidStateBucketProviderIdentifier {
        bucket_name: String,
        error: carina_core::provider::EmptyAppliedIdentifier,
    },

    /// Identity assignment was incomplete at a resolved-only pipeline boundary.
    #[error(transparent)]
    PendingResourceIdentity(#[from] carina_core::resource::PendingResourceIdentityError),

    /// A resource's authored lifecycle paths could not cross the
    /// schema-resolved planning boundary.
    #[error(transparent)]
    ResolvedResource(Box<carina_core::resource::ResolvedResourceError>),

    /// Persisted current state did not agree with the validated move source.
    #[error(transparent)]
    CurrentMoveRouting(#[from] carina_core::differ::CurrentMoveRoutingMismatch),

    /// A state-row move could not find its source in the addressed
    /// current/orphan bundle.
    #[error("cannot apply resource move {from} -> {to}: source state is not loaded")]
    MoveSourceNotLoaded {
        from: Box<ResolvedResourceId>,
        to: Box<ResolvedResourceId>,
    },

    /// A detected state-row move overlaps an already-applied move in a shape
    /// that cannot be represented as one forward chain.
    #[error(
        "cannot apply detected resource move {from} -> {to}: it conflicts with existing move {existing_from} -> {existing_to}"
    )]
    DetectedMoveConflict {
        from: Box<ResolvedResourceId>,
        to: Box<ResolvedResourceId>,
        existing_from: Box<ResolvedResourceId>,
        existing_to: Box<ResolvedResourceId>,
    },

    /// Routed ids did not agree with the operator-authored `moved` block.
    #[error(transparent)]
    ResolvedMove(#[from] carina_core::effect::ResolvedMoveMismatch),

    /// State backend errors (lock contention, I/O, serialization, etc.)
    #[error(transparent)]
    Backend(#[from] BackendError),

    /// `state forget` was asked to remove anything other than an
    /// outcome-unknown create row.
    #[error(transparent)]
    StateForget(Box<ProjectStateForgetError>),

    /// Locked state no longer agrees with the saved plan's create status.
    #[error(transparent)]
    LockedStateReload(Box<ProjectLockedStateReloadError>),

    /// A provider or desired value could not be encoded into one resource row.
    #[error(transparent)]
    ResourceStateEncoding(Box<LocatedResourceStateEncodingError>),

    /// A recorded state-only directive target no longer exists in the
    /// resolved desired-resource set supplied to writeback.
    #[error("directive change target '{resource_id}' is absent from desired resources")]
    PersistedDirectiveTargetMissingFromDesired {
        resource_id: Box<ResolvedResourceId>,
    },

    /// A recorded state-only directive target no longer exists at its exact
    /// provider route in locked state.
    #[error("directive change target '{resource_id}' is absent from locked state")]
    PersistedDirectiveTargetMissingFromLockedState {
        resource_id: Box<ResolvedResourceId>,
    },

    /// A state backend error with a project-scoped recovery command.
    #[error(transparent)]
    ProjectBackend(Box<ProjectBackendError>),

    /// A state-backend error while resolving a concrete upstream project.
    #[error(transparent)]
    UpstreamStateSource(Box<UpstreamStateSourceError>),

    /// Provider errors (AWS API failures, timeouts, etc.)
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Host-side AWS credential resolution failed before a guest started.
    #[error(transparent)]
    AwsCredentials(#[from] AwsCredentialStartupError),

    /// A resource's initialized provider and owner schema could not be paired
    /// for type-aware comparison.
    #[error(transparent)]
    ComparisonResolution(#[from] carina_core::provider::ComparisonResolutionError),

    /// Anonymous state reconciliation found no safe unique legacy row.
    #[error(transparent)]
    AnonymousIdentifierReconciliation(
        #[from] Box<carina_core::identifier::AnonymousIdentifierReconciliationError>,
    ),

    /// A module argument constraint became decidable during an operation.
    #[error(transparent)]
    ModuleConstraint(#[from] carina_core::executor::ModuleConstraintGateError),

    /// Destroy scheduling could not resolve a required dependency target.
    #[error(transparent)]
    DestroyDependencyAnalysis(#[from] DestroyDependencyAnalysisError),

    /// Distinct desired resources resolved to the same execution identity.
    #[error(transparent)]
    DuplicateResolvedResourceId(#[from] crate::wiring::DuplicateResolvedResourceIdError),

    /// A provider-boundary check failed for a resource before dispatch.
    #[error(transparent)]
    ProviderPreparation(#[from] ResourceProviderPreparationError),

    /// Provider configuration failed its construction-boundary checks.
    #[error(transparent)]
    ProviderConfigPreparation(#[from] carina_core::executor::ProviderConfigPreparationError),

    /// Desired resource or data-source enum canonicalization failed.
    #[error(transparent)]
    EnumCanonicalization(#[from] carina_core::value::EnumCanonicalizationError),

    /// Provider configuration enum canonicalization failed.
    #[error(transparent)]
    ProviderConfigCanonicalization(#[from] carina_core::value::ProviderConfigCanonicalizationError),

    /// A source-loaded provider reported a kind other than the block that
    /// selected its artifact.
    #[error(
        "Provider block kind '{configured}' does not match loaded provider name '{loaded}' from source '{provider_source}'."
    )]
    SourceProviderKindMismatch {
        configured: String,
        loaded: String,
        provider_source: String,
    },

    /// A source trusted for one provider identity was selected by an
    /// incompatible provider block kind.
    #[error(
        "Provider block kind '{configured}' does not match trusted provider identity '{expected}' from source '{provider_source}'."
    )]
    SourceProviderTrustMismatch {
        configured: String,
        expected: String,
        provider_source: String,
    },

    /// A parsed provider expression could not be evaluated.
    #[error("Provider attribute resolution error: {0}")]
    ProviderAttributeResolution(Box<carina_core::parser::ParseError>),

    /// Provider resolution completed but left a value outside the resolved invariant.
    #[error("Provider configuration did not fully resolve: {0}")]
    ProviderConfigInvariant(Box<carina_core::parser::ProviderConfigInvariantError>),

    /// An upstream needed by a provider construction attribute could not be loaded.
    #[error("{error}; required by provider '{provider}' attribute '{attribute}'")]
    ProviderReferencedUpstream {
        provider: String,
        attribute: String,
        error: Box<AppError>,
    },

    /// A saved plan attempted to cross the provider boundary with unresolved config.
    #[error("Invalid provider config in saved plan: {0}")]
    SavedProviderConfig(Box<carina_core::parser::ProviderConfigInvariantError>),

    /// A typed saved-plan secret verification failure with actionable cause.
    #[error(transparent)]
    SavedPlanSecretVerification(Box<SavedPlanSecretVerificationError>),

    /// State-backend bootstrap preparation failed before provider dispatch.
    #[error(transparent)]
    StateBucketPreparation(StateBucketPreparationError),

    /// The provider rejected state-backend bootstrap creation.
    #[error(transparent)]
    StateBucketCreate(StateBucketCreateError),

    /// The state bucket was created, but its recovery row could not be written.
    #[error(transparent)]
    StateBucketSeed(Box<StateBucketSeedError>),

    /// Provider lock-file loading or constraint errors.
    #[error(transparent)]
    LockConstraint(#[from] carina_provider_resolver::LockConstraintError),

    /// Plan dependency-graph image construction or output failed.
    #[error(transparent)]
    PlanImage(#[from] crate::plan_image::PlanImageError),

    /// Interactive terminal setup, display preparation, or I/O failed.
    #[error(transparent)]
    Tui(#[from] carina_tui::TuiError),

    /// One schema-aware bare-binding failure at its authored source token.
    #[error(transparent)]
    BindingUse(Box<carina_core::validation::BindingUseFinding>),

    /// All schema-aware bare-binding failures from one resolved file.
    #[error(transparent)]
    BindingUseReport(Box<carina_core::validation::BindingUseReport>),

    /// A parser-only placeholder survived module and function expansion.
    #[error("{}", .0.located())]
    LeftoverPlaceholder(carina_core::resource::LeftoverPlaceholder),

    /// All parser-only placeholders rejected by one file expansion boundary.
    #[error("{}", .0.located())]
    LeftoverPlaceholders(carina_core::resource::LeftoverPlaceholders),

    /// Lifecycle directive values must be concrete before consumers see them.
    #[error("{}", .0.located())]
    Expansion(carina_core::resource::ExpansionError),

    /// One core-owned module-call finding at its parser-authored location.
    #[error(transparent)]
    ModuleCall(Box<carina_core::validation::ModuleCallFinding>),

    /// A type-resolution failure without parser location metadata.
    #[error(transparent)]
    TypeResolution(Box<carina_core::validation::TypeResolutionDiagnostic>),

    /// Validation errors (schema mismatch, invalid config, etc.)
    #[error("{0}")]
    Validation(String),

    /// Configuration errors (missing attributes, invalid paths, etc.)
    #[error("{0}")]
    Config(String),

    /// Apply or destroy failed, with every retained failure available to callers.
    #[error(transparent)]
    ExecutionFailed(#[from] ExecutionFailureReport),

    /// Anonymous resource identity could not be assigned safely.
    #[error(transparent)]
    AnonymousIdentity(#[from] carina_core::identifier::AnonymousIdentityError),

    /// The path passed to `apply` does not exist, so its input kind is unknown.
    #[error(
        "Apply input '{}' does not exist.\n{}\nCause: {error}",
        path.display(),
        apply_input_expectation()
    )]
    ApplyInputNotFound {
        path: PathBuf,
        error: std::io::Error,
    },

    /// The path passed to `apply` is a symlink whose target does not exist.
    #[error(
        "Apply input '{}' is a symbolic link whose target does not exist.\n{}\nCause: {error}",
        path.display(),
        apply_input_expectation()
    )]
    ApplyInputDanglingSymlink {
        path: PathBuf,
        error: std::io::Error,
    },

    /// The path passed to `apply` could not be inspected before dispatch.
    #[error("Failed to inspect apply input '{}': {error}", path.display())]
    ApplyInputInspection {
        path: PathBuf,
        error: std::io::Error,
    },

    /// An existing file passed to `apply` was not a serialized Carina plan.
    #[error(
        "File '{}' is not a Carina plan file (or the plan file is corrupted).\n{}\n\
         Detail: {error}",
        path.display(),
        apply_input_expectation()
    )]
    InvalidPlanFile {
        path: PathBuf,
        error: serde_json::Error,
    },

    /// A file passed to `apply` was not syntactically valid JSON.
    #[error(
        "File '{}' could not be parsed as JSON, so it is not a readable Carina plan file.\n{}\n{}\n\
         JSON error: {error}",
        path.display(),
        unreadable_json_plan_guidance(),
        apply_input_expectation()
    )]
    UnreadableJsonPlanFile {
        path: PathBuf,
        error: serde_json::Error,
    },

    /// A saved plan could not be read from the filesystem.
    #[error("Failed to read plan file '{}': {error}", path.display())]
    PlanFileRead {
        path: PathBuf,
        error: std::io::Error,
    },

    /// A recognized saved plan uses a format version this binary cannot apply.
    #[error(
        "Unsupported plan file version: {found} (expected {expected}). {}",
        plan_file_recreation_guidance(replan_command.as_deref())
    )]
    UnsupportedPlanVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
        replan_command: Option<String>,
    },

    /// A recognized current-version plan had a malformed full body.
    #[error(transparent)]
    CorruptPlanFile(Box<CorruptPlanFileError>),

    /// The saved display locator now selects a different filesystem identity.
    #[error(
        "The source directory resolves to a different location than when the plan was made: {source_path}. Re-run `{replan_command}`."
    )]
    SavedPlanSourceChanged {
        source_path: carina_core::paths::DisplayPath,
        replan_command: String,
    },

    #[error(transparent)]
    SavedPlanSourceResolution(Box<SavedPlanSourceResolutionError>),

    /// A saved plan was computed against different provider schemas.
    #[error("{0}")]
    SavedPlanSchemaMismatch(SavedPlanSchemaMismatchError),

    /// A saved plan was computed with different provider artifacts.
    #[error("{0}")]
    SavedPlanProviderArtifactMismatch(SavedPlanProviderArtifactMismatchError),

    /// A saved plan was computed under a different initialized instance scope.
    #[error("{0}")]
    SavedPlanProviderScopeMismatch(Box<SavedPlanProviderScopeMismatchError>),

    /// Locked state implies a different state-only directive update set than
    /// the one approved in the saved plan.
    #[error(transparent)]
    SavedPlanDirectiveChangesMismatch(Box<SavedPlanDirectiveChangesMismatchError>),

    /// Apply completed with one or more partial create results.
    #[error("{0}")]
    PartialSuccess(String),

    /// Operation interrupted by user (Ctrl+C / SIGINT)
    #[error("Operation cancelled by user")]
    Interrupted,

    /// Heartbeat renewal proved that another process can now own the state lock.
    #[error("State lock was lost; state may be behind the cloud. Cause: {error}")]
    StateLockLost { error: BackendError },
}

impl From<crate::commands::init::InitError> for AppError {
    fn from(error: crate::commands::init::InitError) -> Self {
        Self::Init(Box::new(error))
    }
}

impl AppError {
    pub(crate) fn corrupt_plan_file(
        path: PathBuf,
        replan_command: Option<String>,
        source: PlanFileJsonError,
    ) -> Self {
        Self::CorruptPlanFile(Box::new(CorruptPlanFileError::new(
            path,
            replan_command,
            source,
        )))
    }

    pub(crate) fn from_locked_state_reload(
        source: LockedStateReloadError,
        project_dir: &Path,
    ) -> Self {
        Self::LockedStateReload(Box::new(ProjectLockedStateReloadError::new(
            source,
            project_dir,
        )))
    }

    pub(crate) fn from_state_forget(
        source: carina_state::NotOutcomeUnknown,
        project_dir: &Path,
    ) -> Self {
        Self::StateForget(Box::new(ProjectStateForgetError::new(source, project_dir)))
    }

    pub(crate) fn from_resource_state_encoding(
        resource: ResourceId,
        source: carina_state::ResourceStateEncodingError,
    ) -> Self {
        Self::ResourceStateEncoding(Box::new(LocatedResourceStateEncodingError {
            resource,
            source,
        }))
    }

    /// Enrich legacy-state recovery guidance with the command's project path.
    pub fn with_project_dir(self, project_dir: &Path) -> Self {
        match self {
            Self::Backend(source) if source.invalid_resource_identity().is_some() => {
                match carina_core::paths::DisplayPath::new(project_dir) {
                    Ok(project_dir) => Self::ProjectBackend(Box::new(ProjectBackendError::new(
                        source,
                        project_dir,
                    ))),
                    Err(_) => Self::Backend(source),
                }
            }
            other => other,
        }
    }

    pub(crate) fn with_upstream_state_source(self, source_dir: &Path) -> Self {
        match carina_core::paths::DisplayPath::new(source_dir) {
            Ok(source_dir) => {
                Self::UpstreamStateSource(Box::new(UpstreamStateSourceError::new(self, source_dir)))
            }
            Err(_) => self,
        }
    }

    pub fn from_resource_preparation(
        resource: ResourceId,
        source: carina_core::executor::ProviderPreparationError,
    ) -> Self {
        match source {
            carina_core::executor::ProviderPreparationError::ModuleConstraint(source) => {
                Self::ModuleConstraint(source)
            }
            source => Self::ProviderPreparation(ResourceProviderPreparationError {
                resource,
                source: Box::new(source),
            }),
        }
    }

    pub fn from_state_bucket_preparation(
        source: carina_core::executor::ProviderPreparationError,
    ) -> Self {
        match source {
            carina_core::executor::ProviderPreparationError::ModuleConstraint(source) => {
                Self::ModuleConstraint(source)
            }
            source => Self::StateBucketPreparation(StateBucketPreparationError {
                source: Box::new(source),
            }),
        }
    }
}

/// Render an error and every typed source once, one diagnostic per line.
///
/// Callers add presentation such as the CLI's `Error:` prefix at their own
/// boundary. Keeping traversal here also lets collected validation errors
/// retain the same source detail as directly returned command errors.
pub fn format_error_with_source_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        messages.push(current.to_string());
        source = current.source();
    }
    messages.join("\n")
}

impl From<carina_core::resource::ResolvedResourceError> for AppError {
    fn from(error: carina_core::resource::ResolvedResourceError) -> Self {
        Self::ResolvedResource(Box::new(error))
    }
}

impl From<carina_core::resource::ExpansionError> for AppError {
    fn from(error: carina_core::resource::ExpansionError) -> Self {
        Self::Expansion(error)
    }
}

impl From<carina_core::resource::LeftoverPlaceholder> for AppError {
    fn from(error: carina_core::resource::LeftoverPlaceholder) -> Self {
        Self::LeftoverPlaceholder(error)
    }
}

impl From<carina_core::resource::LeftoverPlaceholders> for AppError {
    fn from(error: carina_core::resource::LeftoverPlaceholders) -> Self {
        Self::LeftoverPlaceholders(error)
    }
}

impl From<String> for AppError {
    fn from(s: String) -> Self {
        AppError::Config(s)
    }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        AppError::Config(s.to_string())
    }
}

impl From<carina_core::value::SerializationError> for AppError {
    fn from(e: carina_core::value::SerializationError) -> Self {
        AppError::Config(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependency_analysis_failure() -> carina_core::effect::deps::DependencyAnalysisFailure {
        use std::collections::{HashMap, HashSet};

        use carina_core::effect::deps::{
            ScheduleInputs, UnresolvedResource, build_effect_dependency_analysis,
        };
        use carina_core::effect::{Effect, EffectGeneration};
        use carina_core::resource::ResolvedResourceId;

        let id = ResourceId::with_provider_identity("mock", "test.Widget", "example", None);
        let effect = Effect::Delete {
            id: ResolvedResourceId::try_new(id).expect("fixture identity is resolved"),
            identifier: carina_core::provider::AppliedIdentifier::new("provider-id")
                .expect("fixture provider identifier is non-empty"),
            generation: EffectGeneration::Current,
            directives: Default::default(),
            binding: Some(carina_core::resource::absolute_binding_address_for_test(
                "example",
            )),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::from(["missing-consumer".into()]),
        };
        build_effect_dependency_analysis(
            &[effect],
            &HashMap::<ResourceId, UnresolvedResource>::new(),
            &[],
            ScheduleInputs::Apply,
        )
        .expect_err("missing required target must fail dependency analysis")
    }

    #[test]
    fn from_backend_error() {
        let backend_err = BackendError::Configuration("missing bucket".to_string());
        let app_err: AppError = backend_err.into();
        assert!(matches!(
            app_err,
            AppError::Backend(BackendError::Configuration(_))
        ));
        assert!(app_err.to_string().contains("missing bucket"));
    }

    #[test]
    fn trusted_local_grant_error_preserves_canonicalize_source_chain() {
        let grant_error = carina_provider_resolver::GrantParseError::Canonicalize {
            value: "/missing/provider.wasm=aws".to_string(),
            path: PathBuf::from("/missing/provider.wasm"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing provider"),
        };
        let app_error = AppError::from(grant_error);

        let mut current: &(dyn std::error::Error + 'static) = &app_error;
        let mut found_io_error = false;
        while let Some(source) = current.source() {
            if source.downcast_ref::<std::io::Error>().is_some() {
                found_io_error = true;
                break;
            }
            current = source;
        }

        assert!(found_io_error, "AppError dropped the canonicalize cause");
    }

    #[test]
    fn composition_source_identity_preserves_typed_io_source_without_repeating_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing-source-root");
        let identity_error = carina_core::resource::CompositionSourceIdentityResolver::new(
            &carina_core::paths::ResolvedPath::new(&missing).unwrap(),
        )
        .expect_err("missing source root must fail canonicalization");
        let app_error = AppError::from(identity_error);

        assert!(
            matches!(&app_error, AppError::CompositionSourceIdentity(_)),
            "composition source failure lost its typed AppError variant: {app_error:?}",
        );
        assert_eq!(
            app_error.to_string(),
            format!(
                "failed to canonicalize composition source path `{}`",
                missing.display()
            ),
            "Display must leave the io cause to the source chain",
        );
        let mut current: &(dyn std::error::Error + 'static) = &app_error;
        let mut found_io_error = false;
        while let Some(source) = current.source() {
            if source.downcast_ref::<std::io::Error>().is_some() {
                found_io_error = true;
                break;
            }
            current = source;
        }
        assert!(
            found_io_error,
            "composition identity chain lost the io error: {app_error:?}",
        );
    }

    #[test]
    fn saved_plan_source_resolution_renders_io_cause_once() {
        let error = AppError::SavedPlanSourceResolution(Box::new(SavedPlanSourceResolutionError {
            source_path: carina_core::paths::DisplayPath::new("missing-project").unwrap(),
            replan_command: "carina plan --out plan.json missing-project".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing saved source"),
        }));
        assert!(!error.to_string().contains("missing saved source"));
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .is::<std::io::Error>()
        );
        assert_eq!(
            format_error_with_source_chain(&error)
                .matches("missing saved source")
                .count(),
            1
        );
    }

    #[test]
    fn saved_plan_schema_mismatch_renders_types_as_readable_lines() {
        let error = AppError::SavedPlanSchemaMismatch(SavedPlanSchemaMismatchError {
            providers: BTreeMap::from([(
                "mock".to_string(),
                vec![
                    "managed `test.resource` (changed)".to_string(),
                    "data source `iam.Roles` (missing)".to_string(),
                ],
            )]),
            replan_command: "carina plan --out plan.json".to_string(),
        });

        let rendered = error.to_string();
        assert!(
            rendered.contains("\n  - provider `mock`: managed `test.resource` (changed)")
                && rendered.contains("\n  - provider `mock`: data source `iam.Roles` (missing)"),
            "schema changes must render one per line: {rendered}"
        );
        assert!(
            !rendered.contains("[\"") && !rendered.contains("\\\""),
            "schema changes must not use Debug list formatting: {rendered}"
        );
    }

    #[test]
    fn saved_plan_provider_artifact_mismatch_renders_every_difference_and_guidance() {
        let error =
            AppError::SavedPlanProviderArtifactMismatch(SavedPlanProviderArtifactMismatchError {
                providers: BTreeMap::from([
                    (
                        "aws".to_string(),
                        SavedPlanProviderArtifactDifference {
                            recorded: Some("wasm sha256 aaaa (file)".to_string()),
                            loaded: None,
                        },
                    ),
                    (
                        "mock".to_string(),
                        SavedPlanProviderArtifactDifference {
                            recorded: Some("built-in".to_string()),
                            loaded: Some("wasm sha256 bbbb (file)".to_string()),
                        },
                    ),
                ]),
                replan_command: "carina plan --out plan.json .".to_string(),
            });

        let rendered = error.to_string();
        assert!(rendered.contains(
            "provider `aws`: recorded wasm sha256 aaaa (file); loaded not loaded (see the provider load error above)"
        ));
        assert!(
            rendered.contains("provider `mock`: recorded built-in; loaded wasm sha256 bbbb (file)")
        );
        assert!(rendered.ends_with(
            "Re-run `carina plan --out plan.json .` to produce a new saved plan before applying."
        ));
    }

    #[test]
    fn from_provider_error() {
        let provider_err = ProviderError::timeout("timeout");
        let app_err: AppError = provider_err.into();
        assert!(matches!(app_err, AppError::Provider(_)));
        assert!(app_err.to_string().contains("timeout"));
    }

    #[test]
    fn validation_error() {
        let app_err = AppError::Validation("invalid region".to_string());
        assert_eq!(app_err.to_string(), "invalid region");
    }

    #[test]
    fn located_binding_use_is_terminal_and_keeps_its_typed_resolution_source() {
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

        let parsed = carina_core::parser::parse_file(
            "let source = mock.test.Source { name = \"source\" }\n\
             mock.test.Consumer { name = source }\n",
            std::path::Path::new("test.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("parse binding-use fixture");
        let mut schemas = SchemaRegistry::new();
        for resource_type in ["test.Source", "test.Consumer"] {
            schemas.insert(
                "mock",
                ResourceSchema::new(resource_type)
                    .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                    .validate()
                    .expect("valid fixture schema"),
            );
        }
        let module_signatures = carina_core::module_resolver::ResolvedModuleSignatures::new();
        let provider_context = carina_core::parser::ProviderContext::default();
        let context = carina_core::validation::ValidationTypeContext::new_providerless(
            &parsed,
            &schemas,
            &provider_context,
            &module_signatures,
        );
        let finding = carina_core::validation::check_bare_binding_uses(&parsed, &context)
            .into_findings()
            .into_iter()
            .next()
            .expect("bare structural binding finding");
        let app_err = AppError::BindingUse(Box::new(finding));

        assert!(app_err.to_string().contains("test.crn"));
        assert!(app_err.to_string().contains("at line 2"));
        assert!(
            !app_err.to_string().contains("'source' is a resource"),
            "the transparent wrapper must keep the typed source out of Display",
        );
        let resolution = std::error::Error::source(&app_err)
            .expect("the transparent AppError must retain the typed resolution source");
        assert!(
            resolution
                .downcast_ref::<carina_core::validation::BindingResolutionError>()
                .is_some(),
            "the located finding is the terminal Display, while its typed resolution remains the source",
        );
    }

    #[test]
    fn destroy_dependency_analysis_keeps_typed_source_and_context_only_display() {
        let app_err = AppError::DestroyDependencyAnalysis(DestroyDependencyAnalysisError::new(
            dependency_analysis_failure(),
        ));

        assert_eq!(
            app_err.to_string(),
            "Failed to analyze destroy dependencies"
        );
        let source = std::error::Error::source(&app_err)
            .expect("dependency analysis failure must remain in the source chain");
        assert!(
            source
                .downcast_ref::<carina_core::effect::deps::DependencyAnalysisFailure>()
                .is_some()
        );
        assert!(source.to_string().contains("missing-consumer"));
    }

    #[test]
    fn config_error() {
        let app_err = AppError::Config("missing path".to_string());
        assert_eq!(app_err.to_string(), "missing path");
    }

    #[test]
    fn plan_image_error_stays_typed_in_app_error() {
        let plan_image_error =
            crate::plan_image::PlanImageTarget::try_from(PathBuf::from("review.unsupported"))
                .unwrap_err();

        let app_error = AppError::from(plan_image_error);

        assert!(matches!(app_error, AppError::PlanImage(_)));
    }

    #[test]
    fn tui_error_keeps_the_typed_provider_failure_in_the_app_error_chain() {
        let app_error = AppError::from(carina_tui::TuiError::PlanDisplay(
            ProviderError::internal_caused_by(
                "comparison resolution failed",
                std::io::Error::other("typed leaf"),
            ),
        ));

        assert_eq!(app_error.to_string(), "Failed to prepare TUI plan details");
        let provider_error = std::error::Error::source(&app_error)
            .and_then(|source| source.downcast_ref::<ProviderError>())
            .expect("transparent AppError must retain the typed TUI provider source");
        assert!(
            provider_error
                .detail()
                .cause()
                .is_some_and(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        );
    }

    #[test]
    fn from_backend_locked_error() {
        let locked = BackendError::Locked {
            lock_id: "abc".to_string(),
            who: "user@host".to_string(),
            operation: "apply".to_string(),
        };
        let app_err: AppError = locked.into();
        assert!(matches!(
            app_err,
            AppError::Backend(BackendError::Locked { .. })
        ));
    }

    #[test]
    fn from_string() {
        let app_err: AppError = "some error".into();
        assert!(matches!(app_err, AppError::Config(_)));
        assert_eq!(app_err.to_string(), "some error");
    }

    #[test]
    fn from_str() {
        let app_err: AppError = "some error".into();
        assert!(matches!(app_err, AppError::Config(_)));
        assert_eq!(app_err.to_string(), "some error");
    }

    #[test]
    fn interrupted_error() {
        let app_err = AppError::Interrupted;
        assert_eq!(app_err.to_string(), "Operation cancelled by user");
    }

    #[test]
    fn state_bucket_create_display_is_context_only_and_seed_is_in_source_chain() {
        use std::error::Error as _;

        let id = ResourceId::with_provider_identity("mock", "s3.Bucket", "state", None);
        let create = crate::dispatch_fixture_create(
            &id,
            crate::FixtureCreateResult::Untracked {
                error: ProviderError::api_error("create leaf"),
            },
        );
        let error =
            StateBucketCreateError::new(create, Some(AppError::Config("seed leaf".to_string())));

        assert_eq!(error.to_string(), "Failed to create state bucket");
        let mut source = error.source();
        let mut found_seed = false;
        while let Some(current) = source {
            found_seed |= current.downcast_ref::<AppError>().is_some();
            source = current.source();
        }
        assert!(
            found_seed,
            "the recovery-row failure must remain typed in the source chain"
        );
    }

    #[test]
    fn invalid_plan_file_renders_serde_error_once() {
        let source = serde_json::from_value::<Vec<serde_json::Value>>(serde_json::json!({}))
            .expect_err("fixture must not match the expected structure");
        let source_detail = source.to_string();
        let app_err = AppError::InvalidPlanFile {
            path: PathBuf::from("reviewed-plan"),
            error: source,
        };

        let rendered = app_err.to_string();
        assert!(rendered.contains(&source_detail));
        assert_eq!(rendered.matches(&source_detail).count(), 1);
        assert!(std::error::Error::source(&app_err).is_none());
    }

    #[test]
    fn unreadable_json_plan_file_renders_serde_error_once() {
        let source = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("fixture must not be valid JSON");
        let source_detail = source.to_string();
        let app_err = AppError::UnreadableJsonPlanFile {
            path: PathBuf::from("truncated-plan"),
            error: source,
        };

        let rendered = app_err.to_string();
        assert!(rendered.contains(&source_detail));
        assert_eq!(rendered.matches(&source_detail).count(), 1);
        assert!(std::error::Error::source(&app_err).is_none());
    }

    #[test]
    fn dangling_apply_symlink_renders_io_error_once() {
        let app_err = AppError::ApplyInputDanglingSymlink {
            path: PathBuf::from("dangling-plan"),
            error: std::io::Error::new(std::io::ErrorKind::NotFound, "missing target"),
        };

        assert_eq!(app_err.to_string().matches("missing target").count(), 1);
        assert!(std::error::Error::source(&app_err).is_none());
    }

    #[test]
    fn plan_file_read_renders_io_error_once() {
        let app_err = AppError::PlanFileRead {
            path: PathBuf::from("reviewed-plan"),
            error: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };

        assert_eq!(app_err.to_string().matches("denied").count(), 1);
        assert!(std::error::Error::source(&app_err).is_none());
    }

    #[test]
    fn corrupt_plan_file_retains_located_serde_source_chain() {
        let source = serde_json::from_value::<Vec<serde_json::Value>>(serde_json::json!({}))
            .expect_err("fixture must not match the expected structure");
        let source_detail = source.to_string();
        let located_error = PlanFileJsonError::at_root(source);
        let located_detail = format!("{source_detail} at JSON location <root>");
        assert_eq!(located_error.to_string(), located_detail);
        assert_eq!(located_error.json_path(), ".");
        assert_eq!(located_error.classify(), serde_json::error::Category::Data);
        let app_err = AppError::corrupt_plan_file(
            PathBuf::from("reviewed-plan"),
            Some("replan command".to_string()),
            located_error,
        );

        let rendered = format_error_with_source_chain(&app_err);
        // The location wrapper now includes the terminal detail itself, while
        // retaining serde as its typed source. Render each typed layer once.
        assert_eq!(
            rendered,
            format!("{app_err}\n{located_detail}\n{source_detail}"),
        );
        assert!(rendered.contains("reviewed-plan"));
        assert!(rendered.contains("replan command"));
        assert!(rendered.contains("regenerate the plan"));
        let located = std::error::Error::source(&app_err)
            .and_then(|source| source.downcast_ref::<PlanFileJsonError>())
            .expect("corrupt plan must retain the typed located decode error");
        let serde_source = std::error::Error::source(located)
            .and_then(|source| source.downcast_ref::<serde_json::Error>())
            .expect("serde error must remain typed and chained");
        assert!(
            std::ptr::eq(serde_source, located.serde_error()),
            "the serde accessor must expose the original chained error",
        );
    }

    #[test]
    fn implements_std_error() {
        let app_err = AppError::Validation("test".to_string());
        let _: &dyn std::error::Error = &app_err;
    }

    #[cfg(unix)]
    #[test]
    fn upstream_error_preserves_symlink_source_directory() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = AppError::Config("invalid backend".into())
            .with_upstream_state_source(&link)
            .to_string();
        assert!(error.contains(&link.display().to_string()), "{error}");
        assert!(!error.contains(&real.display().to_string()), "{error}");
    }

    // -- account-guard formatter tests (#2407) --

    /// The exact account-guard shape produced by carina-provider-awscc.
    const AWSCC_ALLOWED_MISMATCH: &str = "AWS account ID '019115212452' is not in the \
         provider's allowed_account_ids [\"151116838382\"]. Refusing to operate \
         against this account. Check the AWS credentials in your environment.";

    /// The shape produced by carina-provider-aws's
    /// `account_guard.rs::check_account_id` (no prefix, unquoted ID).
    const AWS_ALLOWED_MISMATCH: &str = "AWS account ID 019115212452 is not in allowed_account_ids [\"151116838382\"]; \
         refusing to operate against this account";

    #[test]
    fn account_guard_formatter_recognizes_awscc_allowed_mismatch() {
        let out = format_account_guard_error(AWSCC_ALLOWED_MISMATCH, Some("aws"))
            .expect("awscc allowed mismatch should match");
        assert!(
            out.contains("AWS account mismatch"),
            "header missing: {out}"
        );
        assert!(out.contains("Provider:    aws"), "provider missing: {out}");
        assert!(
            out.contains("Expected:    151116838382 (allowed_account_ids)"),
            "expected line missing: {out}"
        );
        assert!(
            out.contains("Actual:      019115212452"),
            "actual line missing: {out}"
        );
        assert!(out.contains("Action:"), "action line missing: {out}");
        // Must NOT leak hosting-mechanism details.
        assert!(!out.contains("WASM"), "must not leak WASM detail: {out}");
        assert!(!out.contains("panicked"), "must not surface panic: {out}");
        assert!(
            !out.contains("RUST_BACKTRACE"),
            "must not surface backtrace hint: {out}"
        );
    }

    #[test]
    fn account_guard_formatter_recognizes_aws_allowed_mismatch() {
        let out = format_account_guard_error(AWS_ALLOWED_MISMATCH, Some("aws"))
            .expect("aws allowed mismatch should match");
        assert!(
            out.contains("Expected:    151116838382 (allowed_account_ids)"),
            "expected line missing: {out}"
        );
        assert!(
            out.contains("Actual:      019115212452"),
            "actual line missing: {out}"
        );
    }

    #[test]
    fn account_guard_formatter_recognizes_forbidden_mismatch() {
        let msg = "AWS account ID '019115212452' is listed \
                   in the provider's forbidden_account_ids [\"019115212452\"]. \
                   Refusing to operate against this account. \
                   Check the AWS credentials in your environment.";
        let out = format_account_guard_error(msg, Some("awscc"))
            .expect("forbidden mismatch should match");
        assert!(
            out.contains("Provider:    awscc"),
            "provider missing: {out}"
        );
        assert!(out.contains("forbidden_account_ids"), "kind missing: {out}");
    }

    #[test]
    fn account_guard_formatter_returns_none_for_unrelated_error() {
        // Generic provider init failure (e.g. invalid region, missing
        // creds chain) MUST NOT be coerced into the structured shape —
        // callers fall back to the generic `Error: {msg}` rendering.
        let msg = "failed to load AWS credentials from the environment";
        assert!(format_account_guard_error(msg, Some("aws")).is_none());
    }

    #[test]
    fn account_guard_formatter_returns_none_for_validation_error() {
        let msg = "invalid region 'foo-bar-1'";
        assert!(format_account_guard_error(msg, None).is_none());
    }

    #[test]
    fn account_guard_formatter_handles_multi_id_list() {
        let msg = "AWS account ID '019115212452' is not in the provider's \
                   allowed_account_ids [\"111111111111\", \"222222222222\"]. \
                   Refusing to operate against this account.";
        let out = format_account_guard_error(msg, Some("aws")).expect("multi-id list should match");
        assert!(
            out.contains("Expected:    111111111111, 222222222222 (allowed_account_ids)"),
            "expected list missing: {out}"
        );
    }
}
