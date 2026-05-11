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
    /// The bucket/key/operation labels and the full source-chain
    /// rendering of `source` (e.g. AWS error `code`, message,
    /// request id) make the failure actionable at first glance —
    /// see carina-rs/carina#2603. The earlier `Aws(String)` shape
    /// flattened SDK errors via `.to_string()`, which collapses to
    /// the literal `"service error"` for most `SdkError::ServiceError`
    /// values and drops the entire context chain on the floor.
    #[error("{}", aws_err_display(.0))]
    Aws(Box<AwsError>),

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Structured context attached to a [`BackendError::Aws`].
///
/// Each AWS SDK call site populates `operation` (e.g. `"s3.HeadObject"`)
/// and the relevant resource fields (`bucket`, `key`); `source` keeps
/// the underlying SDK error so the `Display` impl can walk the full
/// chain and surface the AWS error code, message, and request id.
#[derive(Debug)]
pub struct AwsError {
    /// Service-qualified API operation, e.g. `"s3.HeadObject"`,
    /// `"s3.GetObject"`. Always present; chosen by the call site.
    pub operation: &'static str,
    /// S3 bucket the call targeted, when applicable.
    pub bucket: Option<String>,
    /// S3 object key the call targeted, when applicable.
    pub key: Option<String>,
    /// The underlying SDK error chain.
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl AwsError {
    /// Build a new structured AWS error. Use the `bucket`/`key`
    /// builders to attach resource context.
    pub fn new(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            operation,
            bucket: None,
            key: None,
            source: Box::new(source),
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
}

/// Render an [`AwsError`] as a multi-line, fully-expanded message:
/// service/operation, the targeted bucket/key (if known), and the
/// full source-chain text from the SDK error. Used by
/// `BackendError::Aws`'s `Display` impl.
fn aws_err_display(err: &AwsError) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "AWS error");
    let _ = writeln!(out, "  operation: {}", err.operation);
    if let Some(bucket) = &err.bucket {
        let _ = writeln!(out, "  bucket: {}", bucket);
    }
    if let Some(key) = &err.key {
        let _ = writeln!(out, "  key: {}", key);
    }
    // Walk the source chain so the AWS error code, message, request
    // id, and any wrapped causes all surface.
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err.source.as_ref());
    let mut depth = 0usize;
    while let Some(e) = current {
        let _ = writeln!(out, "  cause[{}]: {}", depth, e);
        depth += 1;
        current = e.source();
    }
    // Drop the trailing newline so the formatter ("Error: {}") does
    // not emit a blank line at the very end.
    while out.ends_with('\n') {
        out.pop();
    }
    out
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
            rendered.contains("cause[0]: service error"),
            "outer cause must be visible. got:\n{}",
            rendered
        );
        assert!(
            rendered.contains("AccessDenied"),
            "the deeper source chain (AWS error code) must surface. got:\n{}",
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
