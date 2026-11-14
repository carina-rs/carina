//! Local file backend for state storage
//!
//! This backend stores state in a local JSON file (default: carina.state.json).
//! It uses a .lock file for simple locking mechanism.

use async_trait::async_trait;
use carina_core::paths::{DisplayPath, NoFollowLeaf, ResolvedPath};
#[cfg(any(test, feature = "test-utils"))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::sleep as async_sleep;

use crate::backend::{BackendError, BackendResult, RawState, StateBackend};
use crate::backend_config::{LocalBackendLocation, LocalBackendSettings};
use crate::lock::LockInfo;
use crate::state::{self, LoadedState, MigrationInfo, StateFile, log_state_migration_once};

#[cfg(test)]
type WriteHook = Arc<dyn Fn(&StateFile) + Send + Sync>;

#[cfg(test)]
#[derive(Clone, Copy)]
enum AcquireTestPhase {
    Checking,
    Retrying,
}

/// Local file backend for development and simple use cases
/// Locations carry a pin derived from admitted settings or an explicit native address.
///
/// ```
/// use carina_core::{paths::ResolvedPath, parser::BackendConfig, resource::NoPlaceholder};
/// use carina_state::{BackendSelection, BackendResult};
/// async fn construct(dir: &ResolvedPath, raw: &BackendConfig<NoPlaceholder>) -> BackendResult<()> {
///     let selected = BackendSelection::for_read(dir, Some(raw))?;
///     let _ = selected.resolve_with_drift_warning(|_| {}).await?;
///     Ok(())
/// }
/// ```
/// ```compile_fail
/// use carina_core::paths::ResolvedPath;
/// use carina_state::{backends::LocalBackend, LocalBackendSettings};
/// fn bypass(settings: &LocalBackendSettings, base: &ResolvedPath) {
///     let _ = LocalBackend::from_config(settings, base); // forbidden: the existing constructor is crate-private
/// }
/// ```
/// ```
/// # use carina_core::paths::ResolvedPath;
/// # use carina_state::{backends::LocalBackend, LocalBackendSettings};
/// # fn bypass(settings: &LocalBackendSettings, base: &ResolvedPath) {
/// # }
/// ```
pub struct LocalBackend {
    /// Path to the state file
    state_path: ResolvedPath,
    /// Path to the lock file
    lock_path: NoFollowLeaf,
    recovery_path: NoFollowLeaf,
    /// Tracks the first in-memory state-schema migration observed by
    /// `read_state` on this backend instance, so the warning is emitted
    /// exactly once per backend (carina#3283). `carina plan` reads state
    /// twice per run (T0 snapshot + post-plan drift re-read) and an
    /// unguarded `eprintln!` inside `check_and_migrate` would surface
    /// the warning twice for a single physical file.
    migration_logged: OnceLock<MigrationInfo>,
    /// Serializes state replacements even when the async caller that started
    /// one is cancelled while its blocking filesystem work keeps running.
    /// This history is deliberately instance-local and is never seeded from
    /// disk: a fresh writer such as `migrate_state --force` may legitimately
    /// replace a same-lineage file that has a higher serial.
    state_write: Arc<Mutex<LocalWriteState>>,
    #[cfg(test)]
    write_hook: Option<WriteHook>,
    #[cfg(test)]
    read_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    acquire_hook: Option<Arc<dyn Fn(AcquireTestPhase) + Send + Sync>>,
}

#[derive(Default)]
struct LocalWriteState {
    last_written: Option<StateRevision>,
}

struct StateRevision {
    lineage: String,
    serial: u64,
}

const RECOVERY_CLAIM_TIMEOUT_SECS: i64 = 30;
const LOCK_WRITE_GRACE_PERIOD: Duration = Duration::from_millis(100);

struct RecoveryClaimGuard {
    path: NoFollowLeaf,
}

impl Drop for RecoveryClaimGuard {
    fn drop(&mut self) {
        let _ = self.path.remove();
    }
}

impl LocalBackend {
    /// Default state file name
    pub const DEFAULT_STATE_FILE: &'static str = "carina.state.json";

    /// Create a new LocalBackend with a specific state file path.
    ///
    /// Backend construction outside this crate must go through a typed
    /// [`BackendSelection`](crate::BackendSelection).
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn with_path(state_path: PathBuf) -> Self {
        Self::from_location(
            LocalBackendLocation::for_file(&state_path).expect("admit test state path"),
        )
        .expect("resolve test lock path")
    }

    pub(crate) fn from_location(location: LocalBackendLocation) -> BackendResult<Self> {
        let state_path = location.into_path();
        let lock_path = state_path
            .sibling_with_extension("lock")
            .map_err(|error| BackendError::Io(format!("Failed to locate lock file: {error}")))?;
        let recovery_path = lock_path
            .sibling_with_extension("lock.recover")
            .map_err(|error| {
                BackendError::Io(format!("Failed to locate recovery claim: {error}"))
            })?;
        Ok(Self {
            recovery_path,
            state_path,
            lock_path,
            migration_logged: OnceLock::new(),
            state_write: Arc::new(Mutex::new(LocalWriteState::default())),
            #[cfg(test)]
            write_hook: None,
            #[cfg(test)]
            read_hook: None,
            #[cfg(test)]
            acquire_hook: None,
        })
    }

    #[cfg(test)]
    fn with_write_hook(mut self, hook: impl Fn(&StateFile) + Send + Sync + 'static) -> Self {
        self.write_hook = Some(Arc::new(hook));
        self
    }

    /// Create a LocalBackend from configuration, anchoring relative paths at `base_dir`.
    pub(crate) fn from_config(
        config: &LocalBackendSettings,
        base_dir: &ResolvedPath,
    ) -> BackendResult<Self> {
        let location = config.pin(base_dir)?;
        Self::from_location(location)
    }

    /// The state file's diagnostic spelling.
    pub fn state_path(&self) -> &DisplayPath {
        self.state_path.display()
    }

    fn recovery_path(&self) -> NoFollowLeaf {
        // A sibling of the already-pinned lock, never the project alias.
        self.recovery_path.clone()
    }

    fn create_lock_file(path: &NoFollowLeaf, content: &str) -> std::io::Result<()> {
        path.create_lock(content.as_bytes())
    }

    fn file_written_recently(path: &NoFollowLeaf) -> bool {
        path.metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|elapsed| elapsed < LOCK_WRITE_GRACE_PERIOD)
    }

    async fn remove_lock_if_matches(&self, lock_id: &str) -> BackendResult<()> {
        let content = match self.lock_path.read_to_string_async().await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {}",
                    err
                )));
            }
        };

        let existing_lock = match LockInfo::from_json_str(&content) {
            Ok(lock) => lock,
            Err(_) => return Ok(()),
        };

        if existing_lock.id() == lock_id {
            match self.lock_path.remove_async().await {
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

            let claim_content = match claim_path.read_to_string_async().await {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to read recovery claim: {}",
                        err
                    )));
                }
            };

            let claim_is_stale = match LockInfo::from_json_str(&claim_content) {
                Ok(claim) => claim.is_expired(),
                Err(_) if Self::file_written_recently(&claim_path) => {
                    async_sleep(Duration::from_millis(1)).await;
                    continue;
                }
                Err(_) => true,
            };

            if claim_is_stale {
                match claim_path.remove_async().await {
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
        let claim_content = match claim_path.read_to_string_async().await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read recovery claim: {}",
                    err
                )));
            }
        };

        let claim_is_active = match LockInfo::from_json_str(&claim_content) {
            Ok(claim) => !claim.is_expired(),
            Err(_) if Self::file_written_recently(&claim_path) => return Ok(true),
            Err(_) => false,
        };

        if claim_is_active {
            return Ok(true);
        }

        match claim_path.remove_async().await {
            Ok(()) => Ok(false),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(BackendError::Io(format!(
                "Failed to remove stale recovery claim: {}",
                err
            ))),
        }
    }

    async fn read_state_file(
        &self,
    ) -> BackendResult<Option<(Vec<u8>, carina_core::paths::DisplayPath)>> {
        let state_path = self.state_path.clone();
        #[cfg(test)]
        let read_hook = self.read_hook.clone();
        // Keep the blocking read on the blocking pool, just like tokio::fs::read.
        let read = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(hook) = read_hook {
                hook();
            }
            state_path.read()
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(std::convert::identity);
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(BackendError::Io(format!(
                    "Failed to read state file: {error}"
                )));
            }
        };

        let state_location = self.state_path.display().clone();
        Ok(Some((bytes, state_location)))
    }
}

#[async_trait]
impl StateBackend for LocalBackend {
    async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
        let Some((bytes, state_location)) = self.read_state_file().await? else {
            return Ok(None);
        };
        let outcome = state::check_and_migrate_bytes(&bytes)
            .map_err(|error| error.with_state_read_location(&state_location))?;
        let (state, migration) = outcome.into_parts();
        if let Some(info) = migration {
            log_state_migration_once(
                &self.migration_logged,
                info,
                &self.state_path.display().to_string(),
            );
            Ok(Some(LoadedState::Migrated { state, info }))
        } else {
            Ok(Some(LoadedState::Pristine(state)))
        }
    }

    async fn read_state_bytes(&self) -> BackendResult<Option<RawState>> {
        let Some((bytes, state_location)) = self.read_state_file().await? else {
            return Ok(None);
        };
        Ok(Some(RawState::new(bytes, &state_location)))
    }

    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        let state = state.clone();
        let state_path = self.state_path.clone();
        let state_write = Arc::clone(&self.state_write);
        #[cfg(test)]
        let write_hook = self.write_hook.clone();

        tokio::task::spawn_blocking(move || {
            // Test instrumentation deliberately runs before the mutex so a
            // test can model an already-dispatched old write being overtaken.
            #[cfg(test)]
            if let Some(hook) = write_hook {
                hook(&state);
            }

            let mut write_state = state_write.lock().map_err(|_| {
                BackendError::Io("Local state write mutex was poisoned".to_string())
            })?;

            state
                .validate_unique_identities()
                .map_err(BackendError::InvalidState)?;
            let content = carina_core::utils::pretty_with_newline(&state).map_err(|error| {
                BackendError::Serialization(format!("Failed to serialize state: {error}"))
            })?;

            if write_state
                .last_written
                .as_ref()
                .is_some_and(|last| last.lineage == state.lineage && state.serial <= last.serial)
            {
                // A cancelled spawn_blocking write may finish after its newer
                // replacement. The newer same-lineage state is already the
                // desired durable result, so treating this as success keeps
                // write retries idempotent without weakening lineage checks.
                return Ok(());
            }

            state_path
                .atomic_replace(content.as_bytes())
                .map_err(|error| {
                    BackendError::Io(format!("Failed to replace state file: {error}"))
                })?;

            write_state.last_written = Some(StateRevision {
                lineage: state.lineage.clone(),
                serial: state.serial,
            });
            Ok(())
        })
        .await
        .map_err(|error| BackendError::Io(format!("Local state write task failed: {error}")))?
    }

    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
        let lock = LockInfo::new(operation);
        let content = serde_json::to_string_pretty(&lock)
            .map_err(|e| BackendError::Serialization(format!("Failed to serialize lock: {}", e)))?;
        loop {
            #[cfg(test)]
            if let Some(hook) = &self.acquire_hook {
                hook(AcquireTestPhase::Checking);
            }
            if self.has_active_recovery_claim().await? {
                #[cfg(test)]
                if let Some(hook) = &self.acquire_hook {
                    hook(AcquireTestPhase::Retrying);
                }
                async_sleep(Duration::from_millis(1)).await;
                continue;
            }

            match Self::create_lock_file(&self.lock_path, &content) {
                Ok(()) => {
                    if self.has_active_recovery_claim().await? {
                        self.remove_lock_if_matches(lock.id()).await?;
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

            let current_content = match self.lock_path.read_to_string_async().await {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to read lock file: {}",
                        err
                    )));
                }
            };

            match LockInfo::from_json_str(&current_content) {
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

            let current_content = match self.lock_path.read_to_string_async().await {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(BackendError::Io(format!(
                        "Failed to read lock file: {}",
                        err
                    )));
                }
            };

            match LockInfo::from_json_str(&current_content) {
                Ok(existing_lock) if !existing_lock.is_expired() => {
                    return Err(BackendError::locked(&existing_lock));
                }
                Err(_) if Self::file_written_recently(&self.lock_path) => {
                    async_sleep(Duration::from_millis(1)).await;
                    continue;
                }
                _ => {}
            }

            match self.lock_path.remove_async().await {
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
        let content = match self.lock_path.read_to_string_async().await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::LockNotHeld(
                    "lock file no longer exists".to_string(),
                ));
            }
            Err(error) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {error}"
                )));
            }
        };

        let existing_lock = LockInfo::from_json_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id(),
                existing_lock.id()
            )));
        }

        // Write a renewed lock atomically (write to temp, then rename)
        let renewed = lock.renewed();
        let new_content = serde_json::to_string_pretty(&renewed)
            .map_err(|e| BackendError::Serialization(format!("Failed to serialize lock: {}", e)))?;

        self.lock_path
            .atomic_replace_async(new_content.into_bytes())
            .await
            .map_err(|error| BackendError::Io(format!("Failed to renew lock file: {error}")))?;

        // Re-read and verify our lock was not clobbered by a concurrent process.
        // This closes the TOCTOU window between the initial read and the rename.
        let verify_content =
            self.lock_path.read_to_string_async().await.map_err(|e| {
                BackendError::Io(format!("Failed to verify lock after renewal: {}", e))
            })?;
        let verify_lock = LockInfo::from_json_str(&verify_content).map_err(|e| {
            BackendError::InvalidState(format!("Failed to parse lock after renewal: {}", e))
        })?;
        if verify_lock.id() != renewed.id() {
            return Err(BackendError::LockNotHeld(format!(
                "lock was replaced during renewal: expected {}, found {}",
                renewed.id(),
                verify_lock.id()
            )));
        }

        Ok(renewed)
    }

    async fn write_state_locked(&self, state: &StateFile, lock: &LockInfo) -> BackendResult<()> {
        // Verify the lock is still held by us before writing state
        let content = match self.lock_path.read_to_string_async().await {
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

        let existing_lock = LockInfo::from_json_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id(),
                existing_lock.id()
            )));
        }

        self.write_state(state).await
    }

    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        let content = match self.lock_path.read_to_string_async().await {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::LockNotFound(lock.id().to_owned()));
            }
            Err(err) => {
                return Err(BackendError::Io(format!(
                    "Failed to read lock file: {}",
                    err
                )));
            }
        };

        let existing_lock = LockInfo::from_json_str(&content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse lock file: {}", e)))?;

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockMismatch {
                expected: lock.id().to_owned(),
                actual: existing_lock.id().to_owned(),
            });
        }

        self.lock_path
            .remove_async()
            .await
            .map_err(|e| BackendError::Io(format!("Failed to remove lock file: {}", e)))?;

        Ok(())
    }

    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()> {
        let content = match self.lock_path.read_to_string_async().await {
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

        if let Ok(existing_lock) = LockInfo::from_json_str(&content)
            && existing_lock.id() != lock_id
        {
            return Err(BackendError::LockMismatch {
                expected: lock_id.to_string(),
                actual: existing_lock.id().to_owned(),
            });
        }

        self.lock_path
            .remove_async()
            .await
            .map_err(|e| BackendError::Io(format!("Failed to remove lock file: {}", e)))?;

        Ok(())
    }

    async fn storage_readiness(&self) -> BackendResult<crate::StorageReadiness> {
        Ok(crate::StorageReadiness::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ResourceState;
    use carina_core::resource::{ConcreteValue, NoPlaceholder, Value};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier, Condvar, Mutex};
    use tempfile::tempdir;

    fn local_settings(path: Option<String>) -> LocalBackendSettings {
        let attributes = path
            .map(|path| {
                HashMap::from([(
                    "path".to_string(),
                    Value::<NoPlaceholder>::Concrete(ConcreteValue::String(path.into())),
                )])
            })
            .unwrap_or_default();
        let raw = carina_core::parser::BackendConfig {
            backend_type: "local".to_string(),
            attributes: (attributes).into(),
            attribute_spans: Default::default(),
        };
        match crate::BackendConfig::try_from(&raw).unwrap() {
            crate::BackendConfig::Local(settings) => settings,
            crate::BackendConfig::S3(_) => unreachable!(),
        }
    }

    fn state_temp_files(state_path: &Path) -> Vec<PathBuf> {
        let parent = state_path.parent().unwrap_or_else(|| Path::new("."));
        let temp_prefix = state_path.with_extension("json.tmp.");
        let prefix = temp_prefix
            .file_name()
            .expect("state path has a file name")
            .to_string_lossy();
        std::fs::read_dir(parent)
            .expect("state directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix.as_ref()))
            })
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lock_and_recovery_symlink_leaves_fail_without_changing_state() {
        for extension in ["lock", "lock.recover"] {
            for link_before_construction in [true, false] {
                let dir = tempdir().unwrap();
                let state_path = dir.path().join("carina.state.json");
                let state = StateFile::new();
                let original = serde_json::to_vec(&state).unwrap();
                std::fs::write(&state_path, &original).unwrap();
                let leaf = state_path.with_extension(extension);
                if link_before_construction {
                    std::os::unix::fs::symlink(&state_path, &leaf).unwrap();
                }
                let backend = LocalBackend::with_path(state_path.clone());
                if !link_before_construction {
                    std::os::unix::fs::symlink(&state_path, &leaf).unwrap();
                }

                // wall-clock-ok: real filesystem lock retries need a hang guard for symlink leaves.
                let result =
                    tokio::time::timeout(Duration::from_secs(2), backend.acquire_lock("apply"))
                        .await
                        .expect("a symlink leaf must fail promptly");

                assert_eq!(std::fs::read(&state_path).unwrap(), original);
                let error = result.expect_err("a symlink leaf must reject lock acquisition");
                assert!(matches!(&error, BackendError::Io(_)), "{error:?}");
                assert!(error.to_string().contains("symlink"), "{error}");
                assert!(
                    std::fs::symlink_metadata(&leaf)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(
                    backend
                        .read_state()
                        .await
                        .unwrap()
                        .unwrap()
                        .into_state()
                        .lineage,
                    state.lineage
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn state_file_aliases_contend_on_the_pinned_files_lock_and_recovery_claim() {
        for existing_state in [false, true] {
            let dir = tempdir().unwrap();
            let shared = dir.path().join("shared");
            std::fs::create_dir(&shared).unwrap();
            let target = shared.join("state.json");
            let alias = dir.path().join("alias.json");
            if existing_state {
                std::fs::write(&target, serde_json::to_vec(&StateFile::new()).unwrap()).unwrap();
            }
            std::os::unix::fs::symlink("shared/state.json", &alias).unwrap();
            let aliased = LocalBackend::with_path(alias.clone());
            let direct = LocalBackend::with_path(target.clone());
            assert_eq!(
                aliased.state_path().to_string(),
                alias.display().to_string()
            );

            let lock = aliased.acquire_lock("apply").await.unwrap();
            let competing = direct.acquire_lock("destroy").await;
            assert!(
                matches!(competing, Err(BackendError::Locked { .. })),
                "the same state file must have one lock: {competing:?}"
            );
            assert!(shared.join("state.lock").exists());
            assert!(!dir.path().join("alias.lock").exists());
            direct.release_lock(&lock).await.unwrap();

            let claim = aliased.acquire_recovery_claim().await.unwrap().unwrap();
            assert!(direct.acquire_recovery_claim().await.unwrap().is_none());
            assert!(shared.join("state.lock.recover").exists());
            assert!(!dir.path().join("alias.lock.recover").exists());
            drop(claim);

            let lock = direct.acquire_lock("destroy").await.unwrap();
            assert!(matches!(
                aliased.acquire_lock("apply").await,
                Err(BackendError::Locked { .. })
            ));
            aliased.release_lock(&lock).await.unwrap();
        }
    }

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
        let read_state = read_state.unwrap().into_state();
        assert_eq!(read_state.serial, 1);
    }

    // Pin the byte-level shape so carina.state.json matches the
    // trailing-newline convention used by carina-backend.lock and
    // carina-providers.lock (#2583).
    #[tokio::test]
    async fn state_file_ends_with_trailing_newline() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());
        let mut state_file = StateFile::new();
        state_file.increment_serial();
        backend.write_state(&state_file).await.unwrap();
        let bytes = std::fs::read(&state_path).unwrap();
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "carina.state.json must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    #[tokio::test]
    async fn write_state_reports_duplicate_identity_as_invalid_state() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());
        let resource = ResourceState::new("ec2.Vpc", "network", "aws");
        let state = StateFile::from_unchecked_resources_for_test(vec![resource.clone(), resource]);

        let error = backend
            .write_state(&state)
            .await
            .expect_err("duplicate identity must be rejected before serialization");

        assert!(
            matches!(&error, BackendError::InvalidState(message) if message.contains("duplicate resource identity")
                && message.contains("aws")
                && message.contains("ec2.Vpc")
                && message.contains("network")),
            "unexpected error: {error}"
        );
        assert!(!state_path.exists(), "invalid state must not be written");
    }

    #[tokio::test]
    async fn test_local_backend_locking() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path);

        // Acquire lock
        let lock = backend.acquire_lock("apply").await.unwrap();
        assert_eq!(lock.operation(), "apply");

        // Try to acquire again - should fail
        let result = backend.acquire_lock("plan").await;
        assert!(result.is_err());

        // Release lock
        backend.release_lock(&lock).await.unwrap();

        // Now can acquire again
        let lock2 = backend.acquire_lock("destroy").await.unwrap();
        assert_eq!(lock2.operation(), "destroy");
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
        let config = local_settings(None);

        let base_dir = tempdir().unwrap();
        let backend = LocalBackend::from_config(
            &config,
            &carina_core::paths::ResolvedPath::new(base_dir.path()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::path::PathBuf::from(backend.state_path().to_string()),
            base_dir.path().join("carina.state.json")
        );
    }

    #[tokio::test]
    async fn test_local_backend_relative_path_is_anchored() {
        let cwd = std::env::current_dir().unwrap();
        let base_dir = tempdir().unwrap();
        let config = local_settings(Some("custom.state.json".to_string()));

        let backend = LocalBackend::from_config(
            &config,
            &carina_core::paths::ResolvedPath::new(base_dir.path()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::path::PathBuf::from(backend.state_path().to_string()),
            base_dir.path().join("custom.state.json")
        );
        assert_ne!(
            std::path::PathBuf::from(backend.state_path().to_string()),
            cwd.join("custom.state.json")
        );
    }

    #[tokio::test]
    async fn test_local_backend_absolute_path_is_preserved() {
        let base_dir = tempdir().unwrap();
        let state_path = tempdir().unwrap().path().join("absolute.state.json");
        let config = local_settings(Some(state_path.to_string_lossy().into_owned()));

        let backend = LocalBackend::from_config(
            &config,
            &carina_core::paths::ResolvedPath::new(base_dir.path()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::path::PathBuf::from(backend.state_path().to_string()),
            state_path
        );
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
        let parsed = StateFile::from_json_str(&content).unwrap();
        assert_eq!(parsed.serial, 1);

        // Verify no uniquely named temp file is left behind.
        assert!(
            state_temp_files(&state_path).is_empty(),
            "temp files should be cleaned up"
        );
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
        let parsed = StateFile::from_json_str(&content).unwrap();
        assert_eq!(parsed.serial, 2);

        // Verify no uniquely named temp file is left behind.
        assert!(
            state_temp_files(&state_path).is_empty(),
            "temp files should be cleaned up"
        );
    }

    #[test]
    fn state_temp_file_search_matches_unique_temp_names() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let temp_path = state_path.with_extension("json.tmp.test-uuid");
        std::fs::write(&temp_path, b"incomplete state").unwrap();

        assert_eq!(state_temp_files(&state_path), [temp_path]);
    }

    #[tokio::test]
    async fn failed_write_cleans_up_unique_temp_file() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        std::fs::create_dir(&state_path).unwrap();
        let backend = LocalBackend::with_path(state_path.clone());

        let error = backend.write_state(&StateFile::new()).await.unwrap_err();

        assert!(matches!(error, BackendError::Io(_)));
        assert!(
            state_temp_files(&state_path).is_empty(),
            "failed writes must clean up their uniquely named temp files"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn older_write_finishing_after_newer_write_cannot_regress_state() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let (older_started_tx, older_started_rx) = tokio::sync::oneshot::channel();
        let older_started_tx = Arc::new(Mutex::new(Some(older_started_tx)));
        let release_older = Arc::new((Mutex::new(false), Condvar::new()));

        let backend = Arc::new(
            LocalBackend::with_path(state_path.clone()).with_write_hook({
                let older_started_tx = Arc::clone(&older_started_tx);
                let release_older = Arc::clone(&release_older);
                move |state| {
                    if state.serial != 1 {
                        return;
                    }
                    older_started_tx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send(())
                        .unwrap();
                    let (released, wake) = &*release_older;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                }
            }),
        );

        let mut older = StateFile::new();
        older.increment_serial();
        let mut newer = older.clone();
        newer.increment_serial();

        let older_write = {
            let backend = Arc::clone(&backend);
            tokio::spawn(async move { backend.write_state(&older).await })
        };
        older_started_rx.await.unwrap();

        backend.write_state(&newer).await.unwrap();
        {
            let (released, wake) = &*release_older;
            *released.lock().unwrap() = true;
            wake.notify_one();
        }
        older_write.await.unwrap().unwrap();

        let persisted = backend.read_state().await.unwrap().unwrap().into_state();
        assert_eq!(persisted.lineage, newer.lineage);
        assert_eq!(persisted.serial, newer.serial);
    }

    #[tokio::test]
    async fn fresh_backend_can_replace_higher_serial_on_disk_with_lower_serial() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");

        let mut lower = StateFile::new();
        lower.increment_serial();
        let mut higher = lower.clone();
        higher.increment_serial();

        LocalBackend::with_path(state_path.clone())
            .write_state(&higher)
            .await
            .unwrap();
        LocalBackend::with_path(state_path.clone())
            .write_state(&lower)
            .await
            .unwrap();

        let persisted = LocalBackend::with_path(state_path)
            .read_state()
            .await
            .unwrap()
            .unwrap()
            .into_state();
        assert_eq!(persisted.lineage, lower.lineage);
        assert_eq!(persisted.serial, lower.serial);
    }

    #[tokio::test]
    async fn test_renew_lock_refreshes_expiration() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path);

        let lock = backend.acquire_lock("apply").await.unwrap();
        let renewed = backend.renew_lock(&lock).await.unwrap();

        assert_eq!(renewed.id(), lock.id());
        assert_eq!(renewed.operation(), lock.operation());
        assert!(renewed.expires() > lock.created());
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
    async fn test_renew_lock_fails_with_lock_not_held_when_lock_file_is_missing() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        let lock = backend.acquire_lock("apply").await.unwrap();
        std::fs::remove_file(state_path.with_extension("lock")).unwrap();

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

        let read_state = backend.read_state().await.unwrap().unwrap().into_state();
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

    #[tokio::test]
    async fn storage_readiness_is_ready_without_creating_local_storage() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("missing").join("test.state.json");
        let backend = LocalBackend::with_path(state_path.clone());

        match backend.storage_readiness().await.unwrap() {
            crate::StorageReadiness::Ready => {}
            crate::StorageReadiness::Provisionable(_) => {
                panic!("local storage cannot provision S3")
            }
            crate::StorageReadiness::Unavailable(error) => {
                panic!("local storage is ready: {error}")
            }
        }
        assert!(!state_path.parent().unwrap().exists());
    }

    /// A retry must yield before starting the next filesystem check.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_async_operations_do_not_block_runtime() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempdir().unwrap();
        let retrying = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let mut backend = LocalBackend::with_path(dir.path().join("test.state.json"));
        let waiting = retrying.clone();
        let notify = entered.clone();
        backend.acquire_hook = Some(Arc::new(move |phase| match phase {
            AcquireTestPhase::Checking => assert!(
                !waiting.load(Ordering::SeqCst),
                "lock retry reached the next iteration without yielding"
            ),
            AcquireTestPhase::Retrying => {
                waiting.store(true, Ordering::SeqCst);
                notify.notify_one();
            }
        }));
        let lock = backend.acquire_lock("apply").await.unwrap();
        // An active recovery claim keeps acquire pending; an ordinary held lock
        // alone returns Locked rather than entering the retry sleep.
        backend
            .recovery_path()
            .atomic_replace(&serde_json::to_vec(&LockInfo::for_test("recover")).unwrap())
            .unwrap();
        let acquire = backend.acquire_lock("plan");
        tokio::pin!(acquire);
        let concurrent = tokio::spawn(async move {
            entered.notified().await;
        });
        tokio::pin!(concurrent);
        tokio::select! {
            biased;
            outcome = &mut concurrent => outcome.unwrap(),
            result = &mut acquire => panic!("acquire completed while its recovery claim was held: {result:?}"),
        }
        assert!(retrying.load(Ordering::SeqCst));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            acquire.as_mut().poll(&mut context).is_pending(),
            "the concurrent task must finish while acquire is still pending"
        );
        retrying.store(false, Ordering::SeqCst);
        backend.recovery_path().remove().unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(acquire.await, Err(BackendError::Locked { .. })));
        backend.release_lock(&lock).await.unwrap();
    }

    struct BlockingIoGate {
        runtime_thread: std::thread::ThreadId,
        entered: tokio::sync::Notify,
        released: Mutex<bool>,
        wake: Condvar,
    }

    impl Default for BlockingIoGate {
        fn default() -> Self {
            Self {
                runtime_thread: std::thread::current().id(),
                entered: tokio::sync::Notify::new(),
                released: Mutex::new(false),
                wake: Condvar::new(),
            }
        }
    }

    impl BlockingIoGate {
        fn block(&self) {
            assert_ne!(
                std::thread::current().id(),
                self.runtime_thread,
                "filesystem I/O blocked the current-thread runtime"
            );
            self.entered.notify_one();
            let _guard = self
                .wake
                .wait_while(self.released.lock().unwrap(), |released| !*released)
                .unwrap();
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_one();
        }
    }

    async fn concurrent_task_finishes_while_io_is_pending<T>(
        io: impl std::future::Future<Output = T>,
        gate: Arc<BlockingIoGate>,
    ) -> T {
        tokio::pin!(io);
        let probe_gate = gate.clone();
        let concurrent = tokio::spawn(async move {
            probe_gate.entered.notified().await;
        });
        tokio::pin!(concurrent);
        tokio::select! {
            biased;
            outcome = &mut concurrent => outcome.unwrap(),
            _ = &mut io => panic!("I/O completed before the blocked-work probe ran"),
        }
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            io.as_mut().poll(&mut context).is_pending(),
            "the concurrent task must finish while the I/O future is still pending"
        );
        gate.release();
        io.await
    }

    /// Both read and write must leave the current-thread runtime runnable.
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_read_write_does_not_block_runtime() {
        let dir = tempdir().unwrap();
        let write_gate = Arc::new(BlockingIoGate::default());
        let read_gate = Arc::new(BlockingIoGate::default());
        let write_hook = write_gate.clone();
        let mut backend = LocalBackend::with_path(dir.path().join("test.state.json"))
            .with_write_hook(move |_| write_hook.block());
        let read_hook = read_gate.clone();
        backend.read_hook = Some(Arc::new(move || read_hook.block()));
        let mut state = StateFile::new();
        state.increment_serial();
        concurrent_task_finishes_while_io_is_pending(backend.write_state(&state), write_gate)
            .await
            .unwrap();
        let read = concurrent_task_finishes_while_io_is_pending(backend.read_state(), read_gate)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.into_state().serial, state.serial);
    }
}
