use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use carina_core::shutdown::ShutdownToken;
use carina_core::shutdown::testing::{
    TestShutdownTrigger, same_shutdown_channel, shutdown_channel,
};
use carina_state::{
    BackendError, BackendResult, LoadedState, LockInfo, RawState, StateBackend, StateFile,
};
use tempfile::TempDir;

/// Delegates state I/O to a local backend but simulates another process taking
/// the lock after the test confirms its provider call is blocked.
pub(crate) struct LockLossBackend {
    inner: Arc<dyn StateBackend>,
    fail_renewal: tokio::sync::Notify,
    renewal_failed: tokio::sync::Notify,
    locked_writes: AtomicUsize,
    releases: AtomicUsize,
}

impl LockLossBackend {
    fn new(inner: Arc<dyn StateBackend>) -> Self {
        Self {
            inner,
            fail_renewal: tokio::sync::Notify::new(),
            renewal_failed: tokio::sync::Notify::new(),
            locked_writes: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
        }
    }

    pub(crate) fn provider_is_blocked(&self) {
        self.fail_renewal.notify_one();
    }

    pub(crate) async fn wait_for_renewal_failure(&self) {
        // wall-clock-ok: hang guard for the heartbeat reaching the injected LockNotHeld after provider readiness.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.renewal_failed.notified(),
        )
        .await
        .expect("heartbeat did not report the injected renewal failure");
    }

    pub(crate) fn locked_writes(&self) -> usize {
        self.locked_writes.load(Ordering::SeqCst)
    }

    pub(crate) fn releases(&self) -> usize {
        self.releases.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl StateBackend for LockLossBackend {
    async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
        self.inner.read_state().await
    }

    async fn read_state_bytes(&self) -> BackendResult<Option<RawState>> {
        self.inner.read_state_bytes().await
    }

    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        self.inner.write_state(state).await
    }

    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
        self.inner.acquire_lock(operation).await
    }

    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        self.inner.release_lock(lock).await
    }

    async fn renew_lock(&self, _lock: &LockInfo) -> BackendResult<LockInfo> {
        self.fail_renewal.notified().await;
        self.renewal_failed.notify_one();
        Err(BackendError::LockNotHeld(
            "test heartbeat lock was replaced".to_string(),
        ))
    }

    async fn write_state_locked(&self, state: &StateFile, lock: &LockInfo) -> BackendResult<()> {
        self.locked_writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_state_locked(state, lock).await
    }

    async fn write_state_locked_for_cleanup(
        &self,
        state: &StateFile,
        lock: &LockInfo,
    ) -> BackendResult<()> {
        self.locked_writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_state_locked_for_cleanup(state, lock).await
    }

    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()> {
        self.inner.force_unlock(lock_id).await
    }

    async fn bucket_exists(&self) -> BackendResult<bool> {
        self.inner.bucket_exists().await
    }

    async fn create_bucket(&self) -> BackendResult<()> {
        self.inner.create_bucket().await
    }
}

#[derive(Clone)]
pub(crate) struct RefreshDrainBarrier {
    reached: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
    shutdown: ShutdownToken,
}

impl RefreshDrainBarrier {
    pub(crate) async fn reached(&self) {
        self.reached.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

pub(crate) fn install_refresh_drain_barrier(
    runtime: &crate::runtime::CommandRuntime,
    shutdown: ShutdownToken,
) -> RefreshDrainBarrier {
    let barrier = RefreshDrainBarrier {
        reached: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
        shutdown,
    };
    *runtime
        .test_hooks()
        .refresh_drain
        .lock()
        .expect("refresh drain barrier lock") = Some(barrier.clone());
    barrier
}

pub(crate) async fn observe_refresh_drain_wait(
    runtime: &crate::runtime::CommandRuntime,
    shutdown: &ShutdownToken,
) {
    let barrier = {
        let mut barrier = runtime
            .test_hooks()
            .refresh_drain
            .lock()
            .expect("refresh drain barrier lock");
        if barrier
            .as_ref()
            .is_some_and(|barrier| same_shutdown_channel(&barrier.shutdown, shutdown))
        {
            barrier.take()
        } else {
            None
        }
    };
    if let Some(barrier) = barrier {
        barrier.reached.notify_one();
        barrier.release.notified().await;
    }
}

pub(crate) struct CancellationFixtureBase {
    runtime: crate::test_support::TestCommandRuntime,
    _dir: TempDir,
    config_path: PathBuf,
    crn_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    backend: std::sync::Arc<dyn StateBackend>,
    shutdown_trigger: TestShutdownTrigger,
    shutdown: ShutdownToken,
}

impl CancellationFixtureBase {
    pub(crate) fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let config_path = dir.path().to_path_buf();
        let crn_path = dir.path().join("main.crn");
        let state_path = dir.path().join("carina.state.json");
        let lock_path = state_path.with_extension("lock");
        let backend = std::sync::Arc::<dyn StateBackend>::from(
            crate::commands::local_backend_for_test(state_path.clone()),
        );

        let (shutdown_trigger, shutdown) = shutdown_channel();
        Self {
            runtime: crate::test_support::TestCommandRuntime::new(),
            _dir: dir,
            config_path,
            crn_path,
            state_path,
            lock_path,
            backend,
            shutdown_trigger,
            shutdown,
        }
    }

    pub(crate) fn runtime(&self) -> &crate::runtime::CommandRuntime {
        &self.runtime
    }
    pub(crate) fn configure_mock(
        &mut self,
        behavior: carina_provider_mock::MockBehavior,
        scope: carina_provider_mock::MockScope,
    ) {
        self.runtime.set_mock(behavior, scope);
    }
    pub(crate) fn update_mock(
        &mut self,
        update: impl FnOnce(
            &mut carina_provider_mock::MockBehavior,
            &mut carina_provider_mock::MockScope,
        ),
    ) {
        self.runtime.update_mock(update);
    }

    pub(crate) fn write_crn(&self, content: impl AsRef<str>) {
        std::fs::write(&self.crn_path, content.as_ref()).expect("write fixture config");
    }

    pub(crate) fn write_backend_lock(&self) {
        let backend_lock = serde_json::json!({
            "backend_type": "local",
            "attributes": {
                "path": self.state_path.display().to_string(),
            },
        });
        std::fs::write(
            self.config_path.join("carina-backend.lock"),
            format!("{}\n", serde_json::to_string_pretty(&backend_lock).unwrap()),
        )
        .expect("write backend lock");
    }

    pub(crate) fn write_state(&self, state: &StateFile) {
        let state_json = carina_core::utils::pretty_with_newline(state).expect("state json");
        std::fs::write(&self.state_path, state_json).expect("write initial state");
    }

    pub(crate) async fn read_state(&self) -> StateFile {
        self.backend
            .read_state()
            .await
            .expect("read state")
            .expect("state exists")
            .into_state()
    }

    pub(crate) fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub(crate) fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub(crate) fn backend_handle(&self) -> std::sync::Arc<dyn StateBackend> {
        self.backend.clone()
    }

    pub(crate) fn lock_loss_backend_handle(&self) -> Arc<LockLossBackend> {
        Arc::new(LockLossBackend::new(Arc::clone(&self.backend)))
    }

    pub(crate) fn readiness_path(&self, name: &str) -> PathBuf {
        self.config_path.join(name)
    }

    pub(crate) fn shutdown_token(&self) -> ShutdownToken {
        self.shutdown.clone()
    }

    pub(crate) fn shutdown_trigger(&self) -> TestShutdownTrigger {
        self.shutdown_trigger.clone()
    }
}
