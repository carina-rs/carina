//! Typed application error for carina-cli

use std::path::{Path, PathBuf};

use carina_core::hint::ProjectCommand;
use carina_core::provider::ProviderError;
use carina_core::resource::ResourceId;
use carina_state::BackendError;

/// A backend error rendered with the concrete project directory available at
/// the CLI boundary.
#[derive(Debug)]
pub struct ProjectBackendError {
    project_dir: PathBuf,
    source: BackendError,
}

impl ProjectBackendError {
    fn new(source: BackendError, project_dir: &Path) -> Self {
        Self {
            project_dir: project_dir
                .canonicalize()
                .unwrap_or_else(|_| project_dir.to_path_buf()),
            source,
        }
    }
}

impl std::fmt::Display for ProjectBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = self.source.invalid_resource_identity() {
            let plan_command = ProjectCommand::new("plan", &self.project_dir).to_string();
            f.write_str(&error.render_with_plan_command(&plan_command))
        } else {
            std::fmt::Display::fmt(&self.source, f)
        }
    }
}

impl std::error::Error for ProjectBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Provider preparation failure annotated with the same resource header that
/// `ProviderError::for_resource` historically rendered.
#[derive(Debug)]
pub struct ResourceProviderPreparationError {
    resource: ResourceId,
    source: Box<carina_core::executor::ProviderPreparationError>,
}

impl std::fmt::Display for ResourceProviderPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}.{}] {}",
            self.resource.resource_type,
            self.resource.identity_display(),
            self.source
        )
    }
}

impl std::error::Error for ResourceProviderPreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Provider preparation failure while creating the managed state bucket.
#[derive(Debug)]
pub struct StateBucketPreparationError {
    source: Box<carina_core::executor::ProviderPreparationError>,
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

impl std::error::Error for StateBucketPreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
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
    // Both providers wrap the eventual error string; the carina-provider-awscc
    // path tags it with "Provider initialization failed:" before the
    // shared "AWS account ID ..." sentence. Strip that prefix if present
    // so the structured renderer doesn't have to special-case it.
    let stripped = msg
        .strip_prefix("Provider initialization failed: ")
        .unwrap_or(msg)
        .trim();

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

/// Typed error enum for carina-cli operations
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A runtime string was not a valid resource identity.
    #[error(transparent)]
    ResourceIdentity(#[from] carina_core::resource::ResourceIdentityError),

    /// State backend errors (lock contention, I/O, serialization, etc.)
    #[error(transparent)]
    Backend(#[from] BackendError),

    /// A state backend error with a project-scoped recovery command.
    #[error(transparent)]
    ProjectBackend(Box<ProjectBackendError>),

    /// Provider errors (AWS API failures, timeouts, etc.)
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// A module argument constraint became decidable during an operation.
    #[error(transparent)]
    ModuleConstraint(#[from] carina_core::executor::ModuleConstraintGateError),

    /// Distinct desired resources resolved to the same execution identity.
    #[error(transparent)]
    DuplicateResolvedResourceId(
        #[from] carina_core::override_aware::DuplicateResolvedResourceIdError,
    ),

    /// A provider-boundary check failed for a resource before dispatch.
    #[error(transparent)]
    ProviderPreparation(#[from] ResourceProviderPreparationError),

    /// State-backend bootstrap preparation failed before provider dispatch.
    #[error(transparent)]
    StateBucketPreparation(StateBucketPreparationError),

    /// The provider rejected state-backend bootstrap creation.
    #[error("Failed to create state bucket: {source}")]
    StateBucketCreate {
        #[source]
        source: ProviderError,
    },

    /// Provider lock-file loading or constraint errors.
    #[error(transparent)]
    LockConstraint(#[from] carina_provider_resolver::LockConstraintError),

    /// Validation errors (schema mismatch, invalid config, etc.)
    #[error("{0}")]
    Validation(String),

    /// Configuration errors (missing attributes, invalid paths, etc.)
    #[error("{0}")]
    Config(String),

    /// The path passed to `apply` does not exist, so its input kind is unknown.
    #[error(
        "Apply input '{}' does not exist.\n{}",
        path.display(),
        apply_input_expectation()
    )]
    ApplyInputNotFound {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The path passed to `apply` is a symlink whose target does not exist.
    #[error(
        "Apply input '{}' is a symbolic link whose target does not exist.\n{}",
        path.display(),
        apply_input_expectation()
    )]
    ApplyInputDanglingSymlink {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The path passed to `apply` could not be inspected before dispatch.
    #[error("Failed to inspect apply input '{}': {source}", path.display())]
    ApplyInputInspection {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An existing file passed to `apply` was not a serialized Carina plan.
    #[error(
        "File '{}' is not a Carina plan file (or the plan file is corrupted).\n{}\n\
         Detail: {source}",
        path.display(),
        apply_input_expectation()
    )]
    InvalidPlanFile {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A file passed to `apply` was not syntactically valid JSON.
    #[error(
        "File '{}' could not be parsed as JSON, so it is not a readable Carina plan file.\n{}\n{}\n\
         JSON error: {source}",
        path.display(),
        unreadable_json_plan_guidance(),
        apply_input_expectation()
    )]
    UnreadableJsonPlanFile {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A saved plan could not be read from the filesystem.
    #[error("Failed to read plan file '{}': {source}", path.display())]
    PlanFileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
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
    #[error(
        "Failed to parse plan file '{}': {source}\nThe plan file may be corrupted. {}",
        path.display(),
        plan_file_recreation_guidance(replan_command.as_deref())
    )]
    CorruptPlanFile {
        path: PathBuf,
        replan_command: Option<String>,
        #[source]
        source: serde_json::Error,
    },

    /// Apply completed with one or more partial create results.
    #[error("{0}")]
    PartialSuccess(String),

    /// Operation interrupted by user (Ctrl+C / SIGINT)
    #[error("Operation cancelled by user")]
    Interrupted,
}

impl AppError {
    /// Enrich legacy-state recovery guidance with the command's project path.
    pub fn with_project_dir(self, project_dir: &Path) -> Self {
        match self {
            Self::Backend(source) if source.invalid_resource_identity().is_some() => {
                Self::ProjectBackend(Box::new(ProjectBackendError::new(source, project_dir)))
            }
            other => other,
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
    fn config_error() {
        let app_err = AppError::Config("missing path".to_string());
        assert_eq!(app_err.to_string(), "missing path");
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
        let app_err: AppError = "some error".to_string().into();
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
    fn invalid_plan_file_preserves_serde_error_as_source() {
        let source = serde_json::from_value::<Vec<serde_json::Value>>(serde_json::json!({}))
            .expect_err("fixture must not match the expected structure");
        let source_detail = source.to_string();
        let app_err = AppError::InvalidPlanFile {
            path: PathBuf::from("reviewed-plan"),
            source,
        };

        assert!(
            std::error::Error::source(&app_err)
                .is_some_and(|source| source.downcast_ref::<serde_json::Error>().is_some()),
            "serde error must remain available through the error source chain"
        );
        assert!(app_err.to_string().contains(&source_detail));
    }

    #[test]
    fn unreadable_json_plan_file_preserves_serde_error_as_source() {
        let source = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("fixture must not be valid JSON");
        let source_detail = source.to_string();
        let app_err = AppError::UnreadableJsonPlanFile {
            path: PathBuf::from("truncated-plan"),
            source,
        };

        assert!(
            std::error::Error::source(&app_err)
                .is_some_and(|source| source.downcast_ref::<serde_json::Error>().is_some()),
            "serde error must remain available through the error source chain"
        );
        assert!(app_err.to_string().contains(&source_detail));
    }

    #[test]
    fn dangling_apply_symlink_preserves_io_error_as_source() {
        let app_err = AppError::ApplyInputDanglingSymlink {
            path: PathBuf::from("dangling-plan"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing target"),
        };

        assert!(
            std::error::Error::source(&app_err)
                .is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some()),
            "I/O error must remain available through the error source chain"
        );
    }

    #[test]
    fn plan_file_read_preserves_io_error_as_source() {
        let app_err = AppError::PlanFileRead {
            path: PathBuf::from("reviewed-plan"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };

        assert!(
            std::error::Error::source(&app_err)
                .is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some()),
            "I/O error must remain available through the error source chain"
        );
    }

    #[test]
    fn corrupt_plan_file_preserves_serde_error_as_source() {
        let source = serde_json::from_value::<Vec<serde_json::Value>>(serde_json::json!({}))
            .expect_err("fixture must not match the expected structure");
        let app_err = AppError::CorruptPlanFile {
            path: PathBuf::from("reviewed-plan"),
            replan_command: Some("replan command".to_string()),
            source,
        };

        assert!(
            std::error::Error::source(&app_err)
                .is_some_and(|source| source.downcast_ref::<serde_json::Error>().is_some()),
            "serde error must remain available through the error source chain"
        );
    }

    #[test]
    fn implements_std_error() {
        let app_err = AppError::Validation("test".to_string());
        let _: &dyn std::error::Error = &app_err;
    }

    // -- account-guard formatter tests (#2407) --

    /// The exact message shape produced by carina-provider-awscc's
    /// `account_guard.rs::validate_account_against_lists` once it has
    /// been wrapped by `CarinaProvider::initialize` with the
    /// "Provider initialization failed: " prefix.
    const AWSCC_ALLOWED_MISMATCH: &str = "Provider initialization failed: AWS account ID '019115212452' is not in the \
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
        let msg = "Provider initialization failed: AWS account ID '019115212452' is listed \
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
        let msg = "Provider initialization failed: failed to load AWS credentials \
                   from the environment";
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
