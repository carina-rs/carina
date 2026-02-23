//! Local file backend for state storage
//!
//! This backend stores state in a local JSON file (default: carina.state.json).
//! It uses a .lock file for simple locking mechanism.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::backend::{BackendConfig, BackendError, BackendResult, StateBackend};
use crate::lock::LockInfo;
use crate::state::StateFile;

/// Local file backend for development and simple use cases
pub struct LocalBackend {
    /// Path to the state file
    state_path: PathBuf,
    /// Path to the lock file
    lock_path: PathBuf,
}

impl LocalBackend {
    /// Default state file name
    pub const DEFAULT_STATE_FILE: &'static str = "carina.state.json";

    /// Create a new LocalBackend with default paths (carina.state.json in current directory)
    pub fn new() -> Self {
        Self::with_path(PathBuf::from(Self::DEFAULT_STATE_FILE))
    }

    /// Create a new LocalBackend with a specific state file path
    pub fn with_path(state_path: PathBuf) -> Self {
        let lock_path = state_path.with_extension("lock");
        Self {
            state_path,
            lock_path,
        }
    }

    /// Create a LocalBackend from configuration
    pub fn from_config(config: &BackendConfig) -> BackendResult<Self> {
        let path = config
            .get_string("path")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(Self::DEFAULT_STATE_FILE));

        Ok(Self::with_path(path))
    }

    /// Get the state file path
    pub fn state_path(&self) -> &PathBuf {
        &self.state_path
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateBackend for LocalBackend {
    async fn read_state(&self) -> BackendResult<Option<StateFile>> {
        if !self.state_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&self.state_path)
            .map_err(|e| BackendError::Io(format!("Failed to read state file: {}", e)))?;

        let state: StateFile = serde_json::from_str(&content).map_err(|e| {
            BackendError::InvalidState(format!("Failed to parse state file: {}", e))
        })?;

        Ok(Some(state))
    }

    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        let content = serde_json::to_string_pretty(state).map_err(|e| {
            BackendError::Serialization(format!("Failed to serialize state: {}", e))
        })?;

        std::fs::write(&self.state_path, content)
            .map_err(|e| BackendError::Io(format!("Failed to write state file: {}", e)))?;

        Ok(())
    }

    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
        // Check if lock file exists and read it
        if self.lock_path.exists() {
            let content = std::fs::read_to_string(&self.lock_path)
                .map_err(|e| BackendError::Io(format!("Failed to read lock file: {}", e)))?;

            if let Ok(existing_lock) = serde_json::from_str::<LockInfo>(&content) {
                // Check if lock is expired
                if !existing_lock.is_expired() {
                    return Err(BackendError::locked(&existing_lock));
                }
            }
        }

        // Create new lock
        let lock = LockInfo::new(operation);
        let content = serde_json::to_string_pretty(&lock)
            .map_err(|e| BackendError::Serialization(format!("Failed to serialize lock: {}", e)))?;

        std::fs::write(&self.lock_path, content)
            .map_err(|e| BackendError::Io(format!("Failed to write lock file: {}", e)))?;

        Ok(lock)
    }

    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        if !self.lock_path.exists() {
            return Err(BackendError::LockNotFound(lock.id.clone()));
        }

        let content = std::fs::read_to_string(&self.lock_path)
            .map_err(|e| BackendError::Io(format!("Failed to read lock file: {}", e)))?;

        let existing_lock: LockInfo = serde_json::from_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id != lock.id {
            return Err(BackendError::LockMismatch {
                expected: lock.id.clone(),
                actual: existing_lock.id,
            });
        }

        std::fs::remove_file(&self.lock_path)
            .map_err(|e| BackendError::Io(format!("Failed to remove lock file: {}", e)))?;

        Ok(())
    }

    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()> {
        if !self.lock_path.exists() {
            return Err(BackendError::LockNotFound(lock_id.to_string()));
        }

        // Verify lock ID matches
        let content = std::fs::read_to_string(&self.lock_path)
            .map_err(|e| BackendError::Io(format!("Failed to read lock file: {}", e)))?;

        if let Ok(existing_lock) = serde_json::from_str::<LockInfo>(&content)
            && existing_lock.id != lock_id
        {
            return Err(BackendError::LockMismatch {
                expected: lock_id.to_string(),
                actual: existing_lock.id,
            });
        }

        std::fs::remove_file(&self.lock_path)
            .map_err(|e| BackendError::Io(format!("Failed to remove lock file: {}", e)))?;

        Ok(())
    }

    async fn init(&self) -> BackendResult<()> {
        // Local backend doesn't need initialization
        Ok(())
    }

    async fn bucket_exists(&self) -> BackendResult<bool> {
        // For local backend, we consider the "bucket" to always exist
        // (it's just the local filesystem)
        Ok(true)
    }

    async fn create_bucket(&self) -> BackendResult<()> {
        // Local backend doesn't need bucket creation
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_local_backend_read_write() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        // Initially no state
        let state = backend.read_state().await.unwrap();
        assert!(state.is_none());

        // Write state
        let mut state_file = StateFile::new();
        state_file.increment_serial();
        backend.write_state(&state_file).await.unwrap();

        // Read back
        let read_state = backend.read_state().await.unwrap();
        assert!(read_state.is_some());
        let read_state = read_state.unwrap();
        assert_eq!(read_state.serial, 1);
    }

    #[tokio::test]
    async fn test_local_backend_locking() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path);

        // Acquire lock
        let lock = backend.acquire_lock("apply").await.unwrap();
        assert_eq!(lock.operation, "apply");

        // Try to acquire again - should fail
        let result = backend.acquire_lock("plan").await;
        assert!(result.is_err());

        // Release lock
        backend.release_lock(&lock).await.unwrap();

        // Now can acquire again
        let lock2 = backend.acquire_lock("destroy").await.unwrap();
        assert_eq!(lock2.operation, "destroy");
        backend.release_lock(&lock2).await.unwrap();
    }

    #[tokio::test]
    async fn test_local_backend_from_config() {
        use std::collections::HashMap;

        let config = BackendConfig {
            backend_type: "local".to_string(),
            attributes: HashMap::new(),
        };

        let backend = LocalBackend::from_config(&config).unwrap();
        assert_eq!(backend.state_path(), &PathBuf::from("carina.state.json"));
    }

    #[tokio::test]
    async fn test_local_backend_custom_path() {
        use carina_core::resource::Value;
        use std::collections::HashMap;

        let mut attributes = HashMap::new();
        attributes.insert(
            "path".to_string(),
            Value::String("custom.state.json".to_string()),
        );

        let config = BackendConfig {
            backend_type: "local".to_string(),
            attributes,
        };

        let backend = LocalBackend::from_config(&config).unwrap();
        assert_eq!(backend.state_path(), &PathBuf::from("custom.state.json"));
    }

    #[test]
    fn test_local_backend_provider_metadata() {
        let backend = LocalBackend::new();
        assert_eq!(backend.provider_name(), None);
        assert_eq!(backend.resource_type(), None);
        assert_eq!(backend.resource_definition("test"), None);
    }
}
