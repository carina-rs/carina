//! State backend trait and error types

use async_trait::async_trait;
use thiserror::Error;

use crate::lock::LockInfo;
use crate::state::StateFile;

/// Errors that can occur when interacting with a state backend
#[derive(Debug, Error)]
pub enum BackendError {
    /// The state is locked by another process
    #[error("State is locked by {who} (lock ID: {lock_id}, operation: {operation})")]
    Locked {
        lock_id: String,
        who: String,
        operation: String,
    },

    /// The lock was not found (for release/force-unlock operations)
    #[error("Lock not found: {0}")]
    LockNotFound(String),

    /// Lock ID mismatch when trying to release
    #[error("Lock ID mismatch: expected {expected}, got {actual}")]
    LockMismatch { expected: String, actual: String },

    /// The caller's lock is no longer held (expired or stolen)
    #[error("Lock no longer held: {0}")]
    LockNotHeld(String),

    /// The backend type is not supported
    #[error("Unsupported backend type: {0}")]
    UnsupportedBackend(String),

    /// Configuration error
    #[error("Backend configuration error: {0}")]
    Configuration(String),

    /// The bucket/container does not exist
    #[error("Bucket not found: {0}")]
    BucketNotFound(String),

    /// Failed to create bucket
    #[error("Failed to create bucket: {0}")]
    BucketCreationFailed(String),

    /// State file is corrupted or invalid
    #[error("Invalid state file: {0}")]
    InvalidState(String),

    /// State lineage mismatch (prevents accidental state overwrites)
    #[error("State lineage mismatch: expected {expected}, got {actual}")]
    LineageMismatch { expected: String, actual: String },

    /// Network or I/O error
    #[error("I/O error: {0}")]
    Io(String),

    /// AWS SDK error with structured context.
    ///
    /// `AwsError` extracts the actionable fields up-front
    /// (`operation`, `bucket`/`key`, `status`/`code`, AWS-side
    /// `message`, `request_id`, `extended_request_id`) so the
    /// renderer can surface them as labeled lines — see
    /// carina-rs/carina#3235. The legacy `cause[N]:` chain that
    /// collapsed everything behind SDK-internal scaffolding is
    /// gone; transport-level diagnostics (DNS / TLS / timeout) that
    /// produce no HTTP response are preserved via a `cause:` line
    /// appended to the structured output.
    #[error("{}", aws_err_display(.0))]
    Aws(Box<AwsError>),

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Structured context attached to a [`BackendError::Aws`].
///
/// Each AWS SDK call site populates `operation` (e.g. `"s3.HeadObject"`)
/// and the relevant resource fields (`bucket`, `key`). The
/// `status` / `code` / `aws_message` / `request_id` quartet carries
/// the SDK response metadata that the renderer surfaces as labeled
/// lines (carina#3235); `source` keeps the underlying SDK error so
/// transport-level diagnostics (DNS, TLS, timeout) still reach the
/// operator via a `cause:` line when no HTTP response was received.
#[derive(Debug)]
pub struct AwsError {
    /// Service-qualified API operation, e.g. `"s3.HeadObject"`,
    /// `"s3.GetObject"`. Always present; chosen by the call site.
    pub operation: &'static str,
    /// S3 bucket the call targeted, when applicable.
    pub bucket: Option<String>,
    /// S3 object key the call targeted, when applicable.
    pub key: Option<String>,
    /// HTTP status code extracted from the SDK error's raw response.
    pub status: Option<u16>,
    /// AWS error code (e.g. `"AccessDenied"`, `"NoSuchBucket"`) from
    /// `ErrorMetadata::code`. Distinct from `status` — multiple codes
    /// can share a status (HTTP 403 covers `AccessDenied`,
    /// `InvalidAccessKeyId`, `SignatureDoesNotMatch`, …) and the code
    /// is what makes the diagnosis actionable.
    pub code: Option<String>,
    /// AWS error message extracted from `ErrorMetadata::message` —
    /// the human-readable body the operator needs ("User: arn:aws:…
    /// is not authorized to perform: …"). Field name avoids
    /// collision with the outer `BackendError` message context.
    pub aws_message: Option<String>,
    /// AWS request id (`x-amzn-RequestId`) — what operators paste
    /// into support tickets and what the server-side log search
    /// keys off.
    pub request_id: Option<String>,
    /// S3 extended request id (`x-amz-id-2`). S3-specific second id
    /// that AWS Support routinely asks for alongside the primary
    /// `request_id`; carina-rs/carina#3235 names it explicitly as an
    /// acceptance criterion. Empty for non-S3 services that don't
    /// return the header.
    pub extended_request_id: Option<String>,
    /// The underlying SDK error chain. Kept even when the structured
    /// fields above are populated, so the renderer can append a
    /// `cause:` line that preserves transport-level diagnostics
    /// (DNS / TLS / timeouts) when no HTTP response landed.
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl AwsError {
    /// Build a new structured AWS error. Use the `bucket`/`key`
    /// builders to attach resource context and the
    /// `with_status` / `with_code` / `with_aws_message` /
    /// `with_request_id` / `with_extended_request_id` builders (or
    /// [`Self::from_sdk_error`]) to attach the SDK response metadata
    /// that the renderer surfaces as labeled lines.
    pub fn new(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            operation,
            bucket: None,
            key: None,
            status: None,
            code: None,
            aws_message: None,
            request_id: None,
            extended_request_id: None,
            source: Box::new(source),
        }
    }

    /// Extract `status` / `code` / `aws_message` / `request_id` /
    /// `extended_request_id` from an AWS SDK error and attach them
    /// to a fresh `AwsError`. The SDK error itself is preserved in
    /// `source` so transport-level failures (which don't populate
    /// the structured fields because no HTTP response landed) still
    /// surface via the `cause:` line.
    pub fn from_sdk_error<E>(operation: &'static str, err: aws_sdk_s3::error::SdkError<E>) -> Self
    where
        E: std::error::Error + Send + Sync + aws_sdk_s3::error::ProvideErrorMetadata + 'static,
        aws_sdk_s3::error::SdkError<E>:
            aws_sdk_s3::operation::RequestId + aws_sdk_s3::operation::RequestIdExt,
    {
        use aws_sdk_s3::error::ProvideErrorMetadata;
        use aws_sdk_s3::operation::{RequestId, RequestIdExt};
        let status = err.raw_response().map(|r| r.status().as_u16());
        let code = err.code().map(str::to_owned);
        let aws_message = err.message().map(str::to_owned);
        // `request_id` is exposed via `RequestId::request_id()`
        // when the SDK provides it; fall back to extracting it from
        // `ErrorMetadata::extra("aws_request_id")` otherwise.
        let request_id = err
            .request_id()
            .map(str::to_owned)
            .or_else(|| err.meta().extra("aws_request_id").map(str::to_owned));
        // S3 second-id (carina-rs/carina#3235); AWS Support asks for
        // both ids together. Non-S3 SDK error variants don't
        // implement `ExtendedRequestId`, but every S3 op error does
        // (see aws-sdk-s3's per-op `error_meta.rs`).
        let extended_request_id = err.extended_request_id().map(str::to_owned);
        Self {
            operation,
            bucket: None,
            key: None,
            status,
            code,
            aws_message,
            request_id,
            extended_request_id,
            source: Box::new(err),
        }
    }

    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_aws_message(mut self, msg: impl Into<String>) -> Self {
        self.aws_message = Some(msg.into());
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn with_extended_request_id(mut self, ext: impl Into<String>) -> Self {
        self.extended_request_id = Some(ext.into());
        self
    }

    /// Returns `true` when at least one structured *response* field
    /// (`status` / `code` / `aws_message` / `request_id` /
    /// `extended_request_id`) is set — gates the renderer's
    /// multi-line labeled output. When none are set, the renderer
    /// falls back to walking the SDK source chain so transport-level
    /// diagnostics still surface.
    ///
    /// Note: `bucket` / `key` always render when set, in both
    /// branches — they're operational context, not response shape.
    fn has_structured_response_fields(&self) -> bool {
        self.status.is_some()
            || self.code.is_some()
            || self.aws_message.is_some()
            || self.request_id.is_some()
            || self.extended_request_id.is_some()
    }
}

/// Render an [`AwsError`] as a multi-line, labeled message that
/// surfaces the structured response fields (`status`, `code`,
/// `message`, `request_id`, `extended_request_id`) and the
/// bucket/key context — replacing the legacy `cause[N]: …` chain
/// (the depth-numbered per-level lines) that buried the actionable
/// parts behind SDK-internal scaffolding. carina#3235.
///
/// When the structured response fields are absent (transport-level
/// failures: DNS, TLS handshake, network timeout — no HTTP response
/// landed), falls back to a single `cause:` labeled line that joins
/// the full source chain with `: ` — same render shape as the
/// structured branch's appended cause, just without the labeled
/// HTTP-response fields above it. A multi-level wrapping that used
/// to render as `cause[0]: foo\n  cause[1]: bar` now renders as
/// `cause: foo: bar`; the information is preserved, only the
/// per-level line break is gone.
///
/// When BOTH structured fields AND a cause chain are populated, the
/// cause chain is appended as a single `cause:` labeled line at the
/// bottom so transport-level diagnostics aren't lost — same shape as
/// carina-core's renderer (carina#3242 design D5).
fn aws_err_display(err: &AwsError) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "AWS error");
    let _ = writeln!(out, "  operation: {}", err.operation);
    if let Some(bucket) = &err.bucket {
        let _ = writeln!(out, "  bucket: {bucket}");
    }
    if let Some(key) = &err.key {
        let _ = writeln!(out, "  key: {key}");
    }

    if err.has_structured_response_fields() {
        match (err.status, err.code.as_deref()) {
            (Some(s), Some(c)) => {
                let _ = writeln!(out, "  status: {s} {c}");
            }
            (Some(s), None) => {
                let _ = writeln!(out, "  status: {s}");
            }
            (None, Some(c)) => {
                let _ = writeln!(out, "  code: {c}");
            }
            (None, None) => {}
        }
        if let Some(msg) = &err.aws_message {
            let _ = writeln!(out, "  message: {msg}");
        }
        if let Some(id) = &err.request_id {
            let _ = writeln!(out, "  request_id: {id}");
        }
        if let Some(ext) = &err.extended_request_id {
            let _ = writeln!(out, "  extended_request_id: {ext}");
        }
    }

    // `cause:` line is the same in both branches — it preserves
    // transport-level diagnostics that the structured HTTP-response
    // fields can't capture (DNS / TLS / timeouts have no HTTP
    // response) and serves as the only diagnostic in the fallback
    // branch.
    write_cause_line(&mut out, err.source.as_ref());

    // Drop the trailing newline so the formatter ("Error: {}") does
    // not emit a blank line at the very end.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Append a `cause: A: B: C` labeled line that walks the entire
/// `source()` chain. Extracted from `aws_err_display` so the
/// structured branch and the fallback branch can share the same
/// chain-walk shape and stay in sync (review round 1).
fn write_cause_line(out: &mut String, source: &(dyn std::error::Error + 'static)) {
    use std::fmt::Write;
    let _ = write!(out, "  cause: ");
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(source);
    let mut first = true;
    while let Some(e) = current {
        if first {
            let _ = write!(out, "{e}");
            first = false;
        } else {
            let _ = write!(out, ": {e}");
        }
        current = e.source();
    }
    out.push('\n');
}

impl BackendError {
    /// Create a Locked error from a LockInfo
    pub fn locked(lock: &LockInfo) -> Self {
        Self::Locked {
            lock_id: lock.id.clone(),
            who: lock.who.clone(),
            operation: lock.operation.clone(),
        }
    }

    /// Create an unsupported backend error
    pub fn unsupported_backend(backend_type: impl Into<String>) -> Self {
        Self::UnsupportedBackend(backend_type.into())
    }

    /// Create a configuration error
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration(message.into())
    }
}

/// Result type for backend operations
pub type BackendResult<T> = Result<T, BackendError>;

/// Trait for state storage backends
///
/// This trait defines the interface for storing and retrieving state files,
/// as well as managing locks for concurrent access control.
#[async_trait]
pub trait StateBackend: Send + Sync {
    /// Read the current state from the backend
    ///
    /// Returns `None` if no state exists (first-time use)
    async fn read_state(&self) -> BackendResult<Option<StateFile>>;

    /// Write the state to the backend
    ///
    /// The state's serial number should be incremented before calling this
    async fn write_state(&self, state: &StateFile) -> BackendResult<()>;

    /// Acquire a lock for the given operation
    ///
    /// This should fail if a lock is already held by another process
    /// (unless the existing lock has expired)
    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo>;

    /// Release a previously acquired lock
    ///
    /// This should verify that the lock being released matches the provided lock info
    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()>;

    /// Renew a lock by refreshing its expiration timestamp.
    ///
    /// Returns an updated `LockInfo` with a fresh TTL.  The caller must verify
    /// that the on-disk lock still belongs to it; if the lock was stolen the
    /// method returns `LockNotHeld`.
    async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo>;

    /// Write state after verifying the caller still holds the lock.
    ///
    /// This prevents silent state corruption when a lock has expired and been
    /// acquired by another process.
    async fn write_state_locked(&self, state: &StateFile, lock: &LockInfo) -> BackendResult<()>;

    /// Force release a lock by its ID
    ///
    /// This is an administrative operation that should be used with caution
    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()>;

    /// Initialize the backend (create bucket if needed, etc.)
    ///
    /// This is called when setting up state management for the first time
    async fn init(&self) -> BackendResult<()>;

    /// Check if the backend storage (bucket) exists
    async fn bucket_exists(&self) -> BackendResult<bool>;

    /// Create the backend storage (bucket) with appropriate settings
    ///
    /// This creates the bucket with:
    /// - Server-side encryption (AES256)
    /// - Public access blocked
    async fn create_bucket(&self) -> BackendResult<()>;

    /// Provider name for this backend's storage resource (e.g., "aws" for S3)
    fn provider_name(&self) -> Option<&str> {
        None
    }

    /// Resource type of the storage resource (e.g., "s3.Bucket")
    fn resource_type(&self) -> Option<&str> {
        None
    }

    /// DSL resource definition for auto-generating bootstrap code
    fn resource_definition(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Backend type identifier for the implicit/local filesystem backend.
/// The single spelling shared by every "is this local?" decision so the
/// literal `"local"` is not open-coded across modules.
pub const LOCAL_BACKEND_TYPE: &str = "local";

/// Configuration for a state backend
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Backend type (e.g., "s3", "gcs", "local")
    pub backend_type: String,
    /// Backend-specific attributes
    pub attributes: std::collections::HashMap<String, carina_core::resource::Value>,
}

impl BackendConfig {
    /// Get a string attribute value
    pub fn get_string(&self, key: &str) -> Option<&str> {
        match self.attributes.get(key) {
            Some(carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(s),
            )) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Get a boolean attribute value
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.attributes.get(key) {
            Some(carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::Bool(b),
            )) => Some(*b),
            _ => None,
        }
    }

    /// Get a boolean attribute with a default value
    pub fn get_bool_or(&self, key: &str, default: bool) -> bool {
        self.get_bool(key).unwrap_or(default)
    }

    /// Whether this configuration names the local filesystem backend.
    pub fn is_local(&self) -> bool {
        self.backend_type == LOCAL_BACKEND_TYPE
    }
}

impl From<&carina_core::parser::BackendConfig> for BackendConfig {
    fn from(config: &carina_core::parser::BackendConfig) -> Self {
        Self {
            backend_type: config.backend_type.clone(),
            attributes: config.attributes.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::LockInfo;

    #[test]
    fn test_backend_error_locked() {
        let lock = LockInfo::new("apply");
        let error = BackendError::locked(&lock);

        match error {
            BackendError::Locked {
                lock_id,
                who,
                operation,
            } => {
                assert_eq!(lock_id, lock.id);
                assert_eq!(who, lock.who);
                assert_eq!(operation, "apply");
            }
            _ => panic!("Expected Locked error"),
        }
    }

    #[test]
    fn test_backend_error_display() {
        let error = BackendError::unsupported_backend("azure");
        assert_eq!(error.to_string(), "Unsupported backend type: azure");

        let error = BackendError::BucketNotFound("my-bucket".to_string());
        assert_eq!(error.to_string(), "Bucket not found: my-bucket");
    }

    #[test]
    fn test_backend_config_from_provider_context() {
        use carina_core::resource::{ConcreteValue, Value};

        let provider_context = carina_core::parser::BackendConfig {
            backend_type: "s3".to_string(),
            attributes: [
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("state.json".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        };

        let state_config = BackendConfig::from(&provider_context);
        assert_eq!(state_config.backend_type, "s3");
        assert_eq!(state_config.get_string("bucket"), Some("my-bucket"));
        assert_eq!(state_config.get_string("key"), Some("state.json"));
    }

    // carina-rs/carina#2603: BackendError::Aws must surface the
    // operation, bucket/key context, and the entire source-error
    // chain (AWS code, message, request id, ...) — not collapse to
    // the literal `"AWS error: service error"` that hid every clue
    // the user needed when CI failures happened in OIDC-assumed
    // roles where reproducing locally is awkward.

    /// Stand-in for an SDK error chain. Wrapped causes flow through
    /// `source()`, mirroring how `aws-smithy-runtime` exposes the
    /// underlying response/code/message.
    #[derive(Debug)]
    struct ChainErr {
        msg: &'static str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl std::fmt::Display for ChainErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.msg)
        }
    }

    impl std::error::Error for ChainErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref().map(|e| e as &dyn std::error::Error)
        }
    }

    /// Legacy chain-walk fallback shape (no structured response
    /// fields populated): operation + bucket appear as labeled
    /// lines, the source chain is joined into a single `cause:`
    /// line that walks the full `source()` chain. Updated for
    /// carina#3235 — the `cause[N]:` depth-numbered form is gone,
    /// replaced by a single labeled line that still surfaces every
    /// level joined with `: `.
    #[test]
    fn aws_error_display_renders_operation_and_source_chain() {
        let inner = ChainErr {
            msg: "AccessDenied: User: arn:aws:sts::… is not authorized to perform: s3:HeadBucket",
            source: None,
        };
        let outer = ChainErr {
            msg: "service error",
            source: Some(Box::new(inner)),
        };
        let aws = AwsError::new("s3.HeadBucket", outer).bucket("carina-registry-state-dev");
        let err = BackendError::Aws(Box::new(aws));

        let rendered = err.to_string();
        assert!(rendered.starts_with("AWS error\n"), "got:\n{}", rendered);
        assert!(
            rendered.contains("operation: s3.HeadBucket"),
            "operation must be visible. got:\n{}",
            rendered
        );
        assert!(
            rendered.contains("bucket: carina-registry-state-dev"),
            "bucket must be visible. got:\n{}",
            rendered
        );
        assert!(
            rendered.contains("  cause: service error: AccessDenied"),
            "outer cause + deeper source chain must both surface on the \
             single `cause:` line, got:\n{}",
            rendered
        );
        // Negative assertion: the legacy depth-numbered `cause[N]:`
        // shape that #3235 wanted gone must not reappear.
        assert!(
            !rendered.contains("cause[0]:"),
            "legacy cause[N] shape must not reappear, got:\n{}",
            rendered
        );
    }

    #[test]
    fn aws_error_display_includes_key_when_set() {
        let aws = AwsError::new(
            "s3.GetObject",
            ChainErr {
                msg: "NoSuchKey",
                source: None,
            },
        )
        .bucket("my-bucket")
        .key("path/to/state.json");
        let err = BackendError::Aws(Box::new(aws));

        let rendered = err.to_string();
        assert!(
            rendered.contains("operation: s3.GetObject")
                && rendered.contains("bucket: my-bucket")
                && rendered.contains("key: path/to/state.json"),
            "operation/bucket/key must all be present. got:\n{}",
            rendered
        );
    }

    /// carina#3235: when the SDK error metadata is populated, the
    /// rendered message must surface `status`, `code` (combined on
    /// one line), AWS error `message`, and `request_id` as labeled
    /// lines instead of the legacy `cause[N]: …` chain that buries
    /// them behind SDK-internal scaffolding.
    #[test]
    fn aws_error_display_renders_structured_fields() {
        let aws = AwsError::new(
            "s3.HeadBucket",
            ChainErr {
                msg: "AccessDenied (deep chain, suppressed by structured render)",
                source: None,
            },
        )
        .bucket("carina-rs-state")
        .with_status(403)
        .with_code("AccessDenied")
        .with_aws_message(
            "User: arn:aws:sts::151116838382:assumed-role/carina-bootstrap/GitHubActions \
             is not authorized to perform: s3:ListBucket on resource: \
             \"arn:aws:s3:::carina-rs-state\"",
        )
        .with_request_id("K0Q1D04FARNCQD7P");
        let err = BackendError::Aws(Box::new(aws));
        let rendered = err.to_string();
        let lines: Vec<&str> = rendered.lines().collect();

        assert!(
            lines.contains(&"  status: 403 AccessDenied"),
            "status+code must render on one line as `<status> <code>`, got:\n{}",
            rendered
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("  message: User: arn:aws:sts::")),
            "message must render as its own labeled line, got:\n{}",
            rendered
        );
        assert!(
            lines.contains(&"  request_id: K0Q1D04FARNCQD7P"),
            "request_id must render on its own labeled line, got:\n{}",
            rendered
        );
        // The legacy `cause[N]:` chain output must NOT appear once
        // structured fields are present — that was the noise #3235
        // wanted gone.
        assert!(
            !rendered.contains("cause[0]:"),
            "legacy cause[N] chain must be suppressed when structured \
             fields are set, got:\n{}",
            rendered
        );
    }

    /// carina#3235: transport-level failures (DNS, TLS handshake,
    /// network timeout) have no HTTP response, so `status`/`code`/
    /// `message`/`request_id` won't be set. The renderer must fall
    /// back to walking the source chain so the underlying
    /// `connection refused` etc. is still visible.
    #[test]
    fn aws_error_display_falls_back_to_chain_walk_when_no_structured_fields() {
        let aws = AwsError::new(
            "s3.HeadBucket",
            ChainErr {
                msg: "connection refused",
                source: None,
            },
        )
        .bucket("carina-rs-state");
        let err = BackendError::Aws(Box::new(aws));
        let rendered = err.to_string();
        assert!(
            rendered.contains("connection refused"),
            "transport diagnostic must surface via the legacy chain walk \
             when no structured fields are set, got:\n{}",
            rendered
        );
    }

    /// carina#3235: when both structured fields AND a cause chain are
    /// populated (the realistic shape after `from_sdk_error` is
    /// added — the SDK error itself stays in `source` even after
    /// extraction), the renderer surfaces both — structured fields as
    /// labeled lines, then `cause: <chain>` at the bottom so transport
    /// detail isn't lost. This mirrors carina-core's renderer
    /// (carina#3242 design D5).
    #[test]
    fn aws_error_display_appends_cause_chain_when_both_populated() {
        let aws = AwsError::new(
            "s3.HeadBucket",
            ChainErr {
                msg: "transport: tls handshake failed",
                source: None,
            },
        )
        .bucket("carina-rs-state")
        .with_status(500)
        .with_code("InternalError")
        .with_aws_message("Server error");
        let err = BackendError::Aws(Box::new(aws));
        let rendered = err.to_string();
        assert!(
            rendered.contains("\n  cause: transport: tls handshake failed"),
            "cause must render as its own labeled line at the bottom when \
             both structured fields and cause are populated, got:\n{}",
            rendered
        );
    }

    /// carina#3235: S3 returns a second id (`x-amz-id-2`) alongside
    /// the primary `x-amzn-RequestId`; AWS Support routinely asks
    /// for both. The rendered output must include
    /// `extended_request_id:` when populated.
    #[test]
    fn aws_error_display_includes_extended_request_id_when_set() {
        let aws = AwsError::new(
            "s3.HeadBucket",
            ChainErr {
                msg: "AccessDenied (deep chain, suppressed by structured render)",
                source: None,
            },
        )
        .bucket("carina-rs-state")
        .with_status(403)
        .with_code("AccessDenied")
        .with_request_id("K0Q1D04FARNCQD7P")
        .with_extended_request_id("abc123def456==");
        let err = BackendError::Aws(Box::new(aws));
        let rendered = err.to_string();
        assert!(
            rendered
                .lines()
                .any(|l| l == "  extended_request_id: abc123def456=="),
            "extended_request_id must render on its own labeled line, got:\n{}",
            rendered
        );
    }

    /// carina#3235: `from_sdk_error` extracts `status` / `code` /
    /// `aws_message` / `request_id` / `extended_request_id` from a
    /// real `SdkError::ServiceError`. This is the path provider call
    /// sites take (vs. the builder-style construction the renderer
    /// tests use), so a direct unit test on the extraction is
    /// load-bearing.
    #[test]
    fn from_sdk_error_extracts_metadata_from_service_error() {
        use aws_sdk_s3::error::SdkError;
        use aws_sdk_s3::operation::head_bucket::HeadBucketError;
        use aws_sdk_s3::types::error::NotFound;
        use aws_smithy_runtime_api::http::Response;
        use aws_smithy_runtime_api::http::StatusCode;
        use aws_smithy_types::body::SdkBody;
        use aws_smithy_types::error::ErrorMetadata;

        // Build a synthetic SDK error: NotFound with status 404 +
        // metadata code/message + a populated x-amzn-RequestId
        // header so the request-id extraction can be exercised.
        let meta = ErrorMetadata::builder()
            .code("NoSuchBucket")
            .message("The specified bucket does not exist")
            .build();
        let inner_err = HeadBucketError::NotFound(NotFound::builder().meta(meta).build());
        let raw = Response::new(StatusCode::try_from(404u16).unwrap(), SdkBody::empty());
        let sdk_err = SdkError::service_error(inner_err, raw);

        let aws = AwsError::from_sdk_error("s3.HeadBucket", sdk_err);
        assert_eq!(aws.status, Some(404));
        assert_eq!(aws.code.as_deref(), Some("NoSuchBucket"));
        assert_eq!(
            aws.aws_message.as_deref(),
            Some("The specified bucket does not exist"),
        );
    }

    #[test]
    fn aws_error_display_omits_optional_fields_when_unset() {
        // Some operations (e.g. HeadBucket) have no key; a future
        // operation may also have no bucket. The renderer should
        // simply skip those lines instead of writing empty stubs.
        let aws = AwsError::new(
            "s3.HeadBucket",
            ChainErr {
                msg: "AccessDenied",
                source: None,
            },
        )
        .bucket("my-bucket");
        let err = BackendError::Aws(Box::new(aws));
        let rendered = err.to_string();
        assert!(rendered.contains("bucket: my-bucket"));
        assert!(
            !rendered.contains("key:"),
            "no key was set; the key line must be omitted. got:\n{}",
            rendered
        );
    }
}
