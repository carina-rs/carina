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

    /// AWS SDK error
    #[error("AWS error: {0}")]
    Aws(String),

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(String),
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
    /// - Versioning enabled (for state history)
    /// - Server-side encryption (AES256)
    /// - Public access blocked
    async fn create_bucket(&self) -> BackendResult<()>;
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
            Some(carina_core::resource::Value::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Get a boolean attribute value
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.attributes.get(key) {
            Some(carina_core::resource::Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// Get a boolean attribute with a default value
    pub fn get_bool_or(&self, key: &str, default: bool) -> bool {
        self.get_bool(key).unwrap_or(default)
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
}
