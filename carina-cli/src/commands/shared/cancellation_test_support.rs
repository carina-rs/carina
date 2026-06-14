use std::path::{Path, PathBuf};

use carina_state::{LocalBackend, StateBackend, StateFile};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

pub(crate) struct CancellationFixtureBase {
    _dir: TempDir,
    config_path: PathBuf,
    crn_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    backend: LocalBackend,
    cancel: CancellationToken,
}

impl CancellationFixtureBase {
    pub(crate) fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let config_path = dir.path().to_path_buf();
        let crn_path = dir.path().join("main.crn");
        let state_path = dir.path().join("carina.state.json");
        let lock_path = state_path.with_extension("lock");
        let backend = LocalBackend::with_path(state_path.clone());

        Self {
            _dir: dir,
            config_path,
            crn_path,
            state_path,
            lock_path,
            backend,
            cancel: CancellationToken::new(),
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

    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
