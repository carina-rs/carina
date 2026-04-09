//! Local file backend for state storage
//!
//! This backend stores state in a local JSON file (default: carina.state.json).
//! It uses a .lock file for simple locking mechanism.

use async_trait::async_trait;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep as async_sleep;

use crate::backend::{BackendConfig, BackendError, BackendResult, StateBackend};
use crate::lock::LockInfo;
use crate::state::{self, StateFile};

/// Local file backend for development and simple use cases
pub struct LocalBackend {
    /// Path to the state file
    state_path: PathBuf,
    /// Path to the lock file
    lock_path: PathBuf,
}

const RECOVERY_CLAIM_TIMEOUT_SECS: i64 = 30;
const LOCK_WRITE_GRACE_PERIOD: Duration = Duration::from_millis(100);

struct RecoveryClaimGuard {
    path: PathBuf,
}

impl Drop for RecoveryClaimGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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

    fn recovery_path(&self) -> PathBuf {
        self.lock_path.with_extension("lock.recover")
    }

    fn create_lock_file(path: &PathBuf, content: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        if let Err(err) = file.write_all(content.as_bytes()) {
            let _ = std::fs::remove_file(path);
            return Err(err);
        }
        if let Err(err) = file.sync_all() {
            let _ = std::fs::remove_file(path);
            return Err(err);
        }
        Ok(())
    }

    fn file_written_recently(path: &PathBuf) -> bool {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|elapsed| elapsed < LOCK_WRITE_GRACE_PERIOD)
    }

    async fn remove_lock_if_matches(&self, lock_id: &str) -> BackendResult<()> {
        let content = match tokio::fs::read_to_string(&self.lock_path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {}",
                    err
                )));
            }
        };

        let existing_lock = match serde_json::from_str::<LockInfo>(&content) {
            Ok(lock) => lock,
            Err(_) => return Ok(()),
        };

        if existing_lock.id == lock_id {
            match tokio::fs::remove_file(&self.lock_path).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to remove lock file: {}",
                        err
                    )));
                }
            }
        }

        Ok(())
    }

    async fn acquire_recovery_claim(&self) -> BackendResult<Option<RecoveryClaimGuard>> {
        let claim_path = self.recovery_path();
        let claim = LockInfo::with_timeout("recover", RECOVERY_CLAIM_TIMEOUT_SECS);
        let content = serde_json::to_string_pretty(&claim).map_err(|e| {
            BackendError::Serialization(format!("Failed to serialize recovery claim: {}", e))
        })?;

        loop {
            match Self::create_lock_file(&claim_path, &content) {
                Ok(()) => return Ok(Some(RecoveryClaimGuard { path: claim_path })),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to create recovery claim: {}",
                        err
                    )));
                }
            }

            let claim_content = match tokio::fs::read_to_string(&claim_path).await {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to read recovery claim: {}",
                        err
                    )));
                }
            };

            let claim_is_stale = match serde_json::from_str::<LockInfo>(&claim_content) {
                Ok(claim) => claim.is_expired(),
                Err(_) if Self::file_written_recently(&claim_path) => {
                    async_sleep(Duration::from_millis(1)).await;
                    continue;
                }
                Err(_) => true,
            };

            if claim_is_stale {
                match tokio::fs::remove_file(&claim_path).await {
                    Ok(()) => continue,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        return Err(BackendError::Io(format!(
                            "Failed to remove stale recovery claim: {}",
                            err
                        )));
                    }
                }
            }

            return Ok(None);
        }
    }

    async fn has_active_recovery_claim(&self) -> BackendResult<bool> {
        let claim_path = self.recovery_path();
        let claim_content = match tokio::fs::read_to_string(&claim_path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read recovery claim: {}",
                    err
                )));
            }
        };

        let claim_is_active = match serde_json::from_str::<LockInfo>(&claim_content) {
            Ok(claim) => !claim.is_expired(),
            Err(_) if Self::file_written_recently(&claim_path) => return Ok(true),
            Err(_) => false,
        };

        if claim_is_active {
            return Ok(true);
        }

        match tokio::fs::remove_file(&claim_path).await {
            Ok(()) => Ok(false),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(BackendError::Io(format!(
                "Failed to remove stale recovery claim: {}",
                err
            ))),
        }
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
        let content = match tokio::fs::read_to_string(&self.state_path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read state file: {}",
                    err
                )));
            }
        };

        let state = state::check_and_migrate(&content)?;

        Ok(Some(state))
    }

    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        let content = serde_json::to_string_pretty(state).map_err(|e| {
            BackendError::Serialization(format!("Failed to serialize state: {}", e))
        })?;

        // Write to a temp file in the same directory, then rename atomically
        let tmp_path = self.state_path.with_extension("json.tmp");

        tokio::fs::write(&tmp_path, content.as_bytes())
            .await
            .map_err(|e| BackendError::Io(format!("Failed to write temp state file: {}", e)))?;

        // Sync the temp file to disk before renaming
        let file = tokio::fs::File::open(&tmp_path).await.map_err(|e| {
            BackendError::Io(format!("Failed to open temp state file for sync: {}", e))
        })?;
        file.sync_all().await.map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            BackendError::Io(format!("Failed to sync temp state file: {}", e))
        })?;

        tokio::fs::rename(&tmp_path, &self.state_path)
            .await
            .map_err(|e| BackendError::Io(format!("Failed to rename temp state file: {}", e)))?;

        // Fsync the parent directory to ensure the rename is durable
        if let Some(parent) = self.state_path.parent()
            && let Ok(dir) = tokio::fs::File::open(parent).await
        {
            let _ = dir.sync_all().await;
        }

        Ok(())
    }

    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
        let lock = LockInfo::new(operation);
        let content = serde_json::to_string_pretty(&lock)
            .map_err(|e| BackendError::Serialization(format!("Failed to serialize lock: {}", e)))?;
        loop {
            if self.has_active_recovery_claim().await? {
                async_sleep(Duration::from_millis(1)).await;
                continue;
            }

            match Self::create_lock_file(&self.lock_path, &content) {
                Ok(()) => {
                    if self.has_active_recovery_claim().await? {
                        self.remove_lock_if_matches(&lock.id).await?;
                        async_sleep(Duration::from_millis(1)).await;
                        continue;
                    }
                    return Ok(lock);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to write lock file: {}",
                        err
                    )));
                }
            }

            let current_content = match tokio::fs::read_to_string(&self.lock_path).await {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to read lock file: {}",
                        err
                    )));
                }
            };

            match serde_json::from_str::<LockInfo>(&current_content) {
                Ok(existing_lock) if !existing_lock.is_expired() => {
                    return Err(BackendError::locked(&existing_lock));
                }
                Err(_) if Self::file_written_recently(&self.lock_path) => {
                    async_sleep(Duration::from_millis(1)).await;
                    continue;
                }
                _ => {}
            }

            let Some(_claim) = self.acquire_recovery_claim().await? else {
                async_sleep(Duration::from_millis(1)).await;
                continue;
            };

            let current_content = match tokio::fs::read_to_string(&self.lock_path).await {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to read lock file: {}",
                        err
                    )));
                }
            };

            match serde_json::from_str::<LockInfo>(&current_content) {
                Ok(existing_lock) if !existing_lock.is_expired() => {
                    return Err(BackendError::locked(&existing_lock));
                }
                Err(_) if Self::file_written_recently(&self.lock_path) => {
                    async_sleep(Duration::from_millis(1)).await;
                    continue;
                }
                _ => {}
            }

            match tokio::fs::remove_file(&self.lock_path).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to remove stale lock file: {}",
                        err
                    )));
                }
            }

            match Self::create_lock_file(&self.lock_path, &content) {
                Ok(()) => return Ok(lock),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to write lock file: {}",
                        err
                    )));
                }
            }
        }
    }

    async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo> {
        // Read the current lock file and verify it still belongs to us
        let content = tokio::fs::read_to_string(&self.lock_path)
            .await
            .map_err(|e| BackendError::Io(format!("Failed to read lock file: {}", e)))?;

        let existing_lock: LockInfo = serde_json::from_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id != lock.id {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id, existing_lock.id
            )));
        }

        // Write a renewed lock atomically (write to temp, then rename)
        let renewed = lock.renewed();
        let new_content = serde_json::to_string_pretty(&renewed)
            .map_err(|e| BackendError::Serialization(format!("Failed to serialize lock: {}", e)))?;

        let tmp_path = self.lock_path.with_extension("lock.renew.tmp");

        tokio::fs::write(&tmp_path, new_content.as_bytes())
            .await
            .map_err(|e| BackendError::Io(format!("Failed to write temp lock file: {}", e)))?;

        let file = tokio::fs::File::open(&tmp_path).await.map_err(|e| {
            BackendError::Io(format!("Failed to open temp lock file for sync: {}", e))
        })?;
        file.sync_all().await.map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            BackendError::Io(format!("Failed to sync temp lock file: {}", e))
        })?;

        tokio::fs::rename(&tmp_path, &self.lock_path)
            .await
            .map_err(|e| BackendError::Io(format!("Failed to rename temp lock file: {}", e)))?;

        // Re-read and verify our lock was not clobbered by a concurrent process.
        // This closes the TOCTOU window between the initial read and the rename.
        let verify_content = tokio::fs::read_to_string(&self.lock_path)
            .await
            .map_err(|e| BackendError::Io(format!("Failed to verify lock after renewal: {}", e)))?;
        let verify_lock: LockInfo = serde_json::from_str(&verify_content).map_err(|e| {
            BackendError::InvalidState(format!("Failed to parse lock after renewal: {}", e))
        })?;
        if verify_lock.id != renewed.id {
            return Err(BackendError::LockNotHeld(format!(
                "lock was replaced during renewal: expected {}, found {}",
                renewed.id, verify_lock.id
            )));
        }

        Ok(renewed)
    }

    async fn write_state_locked(&self, state: &StateFile, lock: &LockInfo) -> BackendResult<()> {
        // Verify the lock is still held by us before writing state
        let content = match tokio::fs::read_to_string(&self.lock_path).await {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::LockNotHeld(
                    "lock file no longer exists".to_string(),
                ));
            }
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {}",
                    err
                )));
            }
        };

        let existing_lock: LockInfo = serde_json::from_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id != lock.id {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id, existing_lock.id
            )));
        }

        self.write_state(state).await
    }

    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        let content = match tokio::fs::read_to_string(&self.lock_path).await {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::LockNotFound(lock.id.clone()));
            }
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {}",
                    err
                )));
            }
        };

        let existing_lock: LockInfo = serde_json::from_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id != lock.id {
            return Err(BackendError::LockMismatch {
                expected: lock.id.clone(),
                actual: existing_lock.id,
            });
        }

        tokio::fs::remove_file(&self.lock_path)
            .await
            .map_err(|e| BackendError::Io(format!("Failed to remove lock file: {}", e)))?;

        Ok(())
    }

    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()> {
        let content = match tokio::fs::read_to_string(&self.lock_path).await {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::LockNotFound(lock_id.to_string()));
            }
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {}",
                    err
                )));
            }
        };

        if let Ok(existing_lock) = serde_json::from_str::<LockInfo>(&content)
            && existing_lock.id != lock_id
        {
            return Err(BackendError::LockMismatch {
                expected: lock_id.to_string(),
                actual: existing_lock.id,
            });
        }

        tokio::fs::remove_file(&self.lock_path)
            .await
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
    use std::sync::{Arc, Barrier};
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

    #[test]
    fn test_local_backend_lock_acquisition_is_atomic() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = Arc::new(LocalBackend::with_path(state_path));
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let backend = Arc::clone(&backend);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Runtime::new().unwrap();
                    barrier.wait();
                    runtime.block_on(backend.acquire_lock("apply"))
                })
            })
            .collect();

        let mut successes = Vec::new();
        let mut failures = 0;

        for handle in handles {
            match handle.join().unwrap() {
                Ok(lock) => successes.push(lock),
                Err(BackendError::Locked { .. }) => failures += 1,
                Err(other) => panic!("unexpected error: {other}"),
            }
        }

        assert_eq!(successes.len(), 1);
        assert_eq!(failures, 7);

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(backend.release_lock(&successes[0]))
            .unwrap();
    }

    #[tokio::test]
    async fn test_local_backend_replaces_expired_lock_once() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = Arc::new(LocalBackend::with_path(state_path.clone()));

        let expired_lock = LockInfo::with_timeout("apply", -60);
        let expired_content = serde_json::to_string_pretty(&expired_lock).unwrap();
        std::fs::write(state_path.with_extension("lock"), expired_content).unwrap();

        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let backend = Arc::clone(&backend);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Runtime::new().unwrap();
                    barrier.wait();
                    runtime.block_on(backend.acquire_lock("apply"))
                })
            })
            .collect();

        let mut successes = Vec::new();
        let mut failures = 0;

        for handle in handles {
            match handle.join().unwrap() {
                Ok(lock) => successes.push(lock),
                Err(BackendError::Locked { .. }) => failures += 1,
                Err(other) => panic!("unexpected error: {other}"),
            }
        }

        assert_eq!(successes.len(), 1);
        assert_eq!(failures, 3);

        backend.release_lock(&successes[0]).await.unwrap();
    }

    #[tokio::test]
    async fn test_local_backend_ignores_stale_recovery_claim() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        let expired_lock = LockInfo::with_timeout("apply", -60);
        let expired_content = serde_json::to_string_pretty(&expired_lock).unwrap();
        std::fs::write(state_path.with_extension("lock"), expired_content).unwrap();

        let stale_claim = LockInfo::with_timeout("recover", -60);
        let stale_claim_content = serde_json::to_string_pretty(&stale_claim).unwrap();
        std::fs::write(
            state_path.with_extension("lock.recover"),
            stale_claim_content,
        )
        .unwrap();

        let lock = backend.acquire_lock("apply").await.unwrap();
        backend.release_lock(&lock).await.unwrap();
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

    #[tokio::test]
    async fn test_write_state_is_atomic() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        // Write state
        let mut state_file = StateFile::new();
        state_file.increment_serial();
        backend.write_state(&state_file).await.unwrap();

        // Verify the state file contains valid JSON (not partial/corrupt)
        let content = std::fs::read_to_string(&state_path).unwrap();
        let parsed: StateFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.serial, 1);

        // Verify no temp file is left behind
        let tmp_path = state_path.with_extension("json.tmp");
        assert!(!tmp_path.exists(), "temp file should be cleaned up");
    }

    #[tokio::test]
    async fn test_write_state_overwrites_existing_atomically() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        // Write initial state
        let mut state_file = StateFile::new();
        state_file.increment_serial();
        backend.write_state(&state_file).await.unwrap();

        // Overwrite with new state
        state_file.increment_serial();
        backend.write_state(&state_file).await.unwrap();

        // Verify the file contains the updated state (not corrupted)
        let content = std::fs::read_to_string(&state_path).unwrap();
        let parsed: StateFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.serial, 2);

        // Verify no temp file is left behind
        let tmp_path = state_path.with_extension("json.tmp");
        assert!(!tmp_path.exists(), "temp file should be cleaned up");
    }

    #[tokio::test]
    async fn test_renew_lock_refreshes_expiration() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path);

        let lock = backend.acquire_lock("apply").await.unwrap();
        let renewed = backend.renew_lock(&lock).await.unwrap();

        assert_eq!(renewed.id, lock.id);
        assert_eq!(renewed.operation, lock.operation);
        assert!(renewed.expires > lock.created);
        assert!(!renewed.is_expired());

        backend.release_lock(&renewed).await.unwrap();
    }

    #[tokio::test]
    async fn test_renew_lock_fails_when_lock_stolen() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        let lock = backend.acquire_lock("apply").await.unwrap();

        // Simulate another process stealing the lock by overwriting the lock file
        let thief_lock = LockInfo::new("destroy");
        let thief_content = serde_json::to_string_pretty(&thief_lock).unwrap();
        std::fs::write(state_path.with_extension("lock"), thief_content).unwrap();

        let result = backend.renew_lock(&lock).await;
        assert!(matches!(result, Err(BackendError::LockNotHeld(_))));
    }

    #[tokio::test]
    async fn test_write_state_locked_succeeds_when_lock_held() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        let lock = backend.acquire_lock("apply").await.unwrap();

        let mut state_file = StateFile::new();
        state_file.increment_serial();
        backend
            .write_state_locked(&state_file, &lock)
            .await
            .unwrap();

        let read_state = backend.read_state().await.unwrap().unwrap();
        assert_eq!(read_state.serial, 1);

        backend.release_lock(&lock).await.unwrap();
    }

    #[tokio::test]
    async fn test_write_state_locked_fails_when_lock_stolen() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        let lock = backend.acquire_lock("apply").await.unwrap();

        // Simulate another process stealing the lock
        let thief_lock = LockInfo::new("destroy");
        let thief_content = serde_json::to_string_pretty(&thief_lock).unwrap();
        std::fs::write(state_path.with_extension("lock"), thief_content).unwrap();

        let mut state_file = StateFile::new();
        state_file.increment_serial();
        let result = backend.write_state_locked(&state_file, &lock).await;
        assert!(matches!(result, Err(BackendError::LockNotHeld(_))));
    }

    #[tokio::test]
    async fn test_write_state_locked_fails_when_lock_file_deleted() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        let lock = backend.acquire_lock("apply").await.unwrap();

        // Remove the lock file to simulate expiration + deletion
        std::fs::remove_file(state_path.with_extension("lock")).unwrap();

        let state_file = StateFile::new();
        let result = backend.write_state_locked(&state_file, &lock).await;
        assert!(matches!(result, Err(BackendError::LockNotHeld(_))));
    }

    #[test]
    fn test_local_backend_provider_metadata() {
        let backend = LocalBackend::new();
        assert_eq!(backend.provider_name(), None);
        assert_eq!(backend.resource_type(), None);
        assert_eq!(backend.resource_definition("test"), None);
    }

    /// Verify that async methods do not block the tokio runtime.
    ///
    /// Uses a single-threaded runtime so that any `std::thread::sleep` inside
    /// the async lock-acquisition loop would starve concurrent tasks.  With
    /// `tokio::time::sleep` the runtime can schedule other tasks while waiting.
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_operations_do_not_block_runtime() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::time::{Duration, timeout};

        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = Arc::new(LocalBackend::with_path(state_path.clone()));

        // Hold a lock so that a second acquire_lock will enter the retry loop
        let lock = backend.acquire_lock("apply").await.unwrap();

        let concurrent_ran = Arc::new(AtomicBool::new(false));
        let concurrent_ran_clone = Arc::clone(&concurrent_ran);

        // Spawn a task that tries to acquire the lock (will loop because lock is held)
        let backend_clone = Arc::clone(&backend);
        let acquire_handle = tokio::spawn(async move {
            // This will fail with Locked error, but the important thing is
            // it must yield to the runtime while retrying
            let _ = backend_clone.acquire_lock("plan").await;
        });

        // Spawn a concurrent task that should be able to run
        let concurrent_handle = tokio::spawn(async move {
            concurrent_ran_clone.store(true, Ordering::SeqCst);
        });

        // Wait for the concurrent task to complete - if the runtime is blocked
        // by std::thread::sleep, this will time out
        let result = timeout(Duration::from_secs(2), concurrent_handle).await;
        assert!(
            result.is_ok(),
            "concurrent task should complete without being blocked"
        );
        assert!(
            concurrent_ran.load(Ordering::SeqCst),
            "concurrent task should have executed"
        );

        // Clean up: abort the acquire task and release the lock
        acquire_handle.abort();
        backend.release_lock(&lock).await.unwrap();
    }

    /// Verify that read_state and write_state use async I/O.
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_read_write_does_not_block_runtime() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::time::{Duration, timeout};

        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = Arc::new(LocalBackend::with_path(state_path));

        let concurrent_ran = Arc::new(AtomicBool::new(false));
        let concurrent_ran_clone = Arc::clone(&concurrent_ran);

        let backend_clone = Arc::clone(&backend);
        let io_handle = tokio::spawn(async move {
            let mut state = StateFile::new();
            state.increment_serial();
            backend_clone.write_state(&state).await.unwrap();
            let read = backend_clone.read_state().await.unwrap();
            assert!(read.is_some());
        });

        let concurrent_handle = tokio::spawn(async move {
            concurrent_ran_clone.store(true, Ordering::SeqCst);
        });

        let result = timeout(Duration::from_secs(2), concurrent_handle).await;
        assert!(result.is_ok());
        assert!(concurrent_ran.load(Ordering::SeqCst));

        let _ = timeout(Duration::from_secs(2), io_handle).await;
    }
}
