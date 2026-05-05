//! Typed application error for carina-cli

use carina_core::provider::ProviderError;
use carina_state::BackendError;

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
    /// State backend errors (lock contention, I/O, serialization, etc.)
    #[error(transparent)]
    Backend(#[from] BackendError),

    /// Provider errors (AWS API failures, timeouts, etc.)
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Validation errors (schema mismatch, invalid config, etc.)
    #[error("{0}")]
    Validation(String),

    /// Configuration errors (missing attributes, invalid paths, etc.)
    #[error("{0}")]
    Config(String),

    /// Operation interrupted by user (Ctrl+C / SIGINT)
    #[error("Operation cancelled by user")]
    Interrupted,
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
        let provider_err = ProviderError::new("timeout");
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
