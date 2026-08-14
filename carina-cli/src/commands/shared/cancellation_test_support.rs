use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use carina_core::shutdown::ShutdownToken;
use carina_core::shutdown::testing::{
    TestShutdownTrigger, same_shutdown_channel, shutdown_channel,
};
use carina_state::{LocalBackend, StateBackend, StateFile};
use tempfile::TempDir;

pub(crate) static MOCK_PROVIDER_ENV_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

static REFRESH_DRAIN_BARRIER: std::sync::Mutex<Option<RefreshDrainBarrier>> =
    std::sync::Mutex::new(None);

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

pub(crate) fn install_refresh_drain_barrier(shutdown: ShutdownToken) -> RefreshDrainBarrier {
    let barrier = RefreshDrainBarrier {
        reached: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
        shutdown,
    };
    *REFRESH_DRAIN_BARRIER
        .lock()
        .expect("refresh drain barrier lock") = Some(barrier.clone());
    barrier
}

pub(crate) async fn observe_refresh_drain_wait(shutdown: &ShutdownToken) {
    let barrier = {
        let mut barrier = REFRESH_DRAIN_BARRIER
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

pub(crate) struct ScopedEnv {
    name: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnv {
    pub(crate) fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: cancellation tests hold MOCK_PROVIDER_ENV_LOCK for the
        // lifetime of every ScopedEnv value.
        unsafe {
            std::env::set_var(name, value);
        }
        Self { name, previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        // SAFETY: cancellation tests hold MOCK_PROVIDER_ENV_LOCK until after
        // their fixture (and therefore these guards) is dropped.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}

pub(crate) struct CancellationFixtureBase {
    _dir: TempDir,
    config_path: PathBuf,
    crn_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    backend: LocalBackend,
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
        let backend = LocalBackend::with_path(state_path.clone());

        let (shutdown_trigger, shutdown) = shutdown_channel();
        Self {
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

    pub(crate) fn backend(&self) -> &LocalBackend {
        &self.backend
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
