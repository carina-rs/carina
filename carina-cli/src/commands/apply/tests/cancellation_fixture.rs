use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use carina_core::executor::{ExecutionEvent, ExecutionObserver};
use carina_core::parser::ProviderContext;
use carina_core::plan::Plan;
use carina_state::{LocalBackend, StateBackend};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::commands::shared::observer::CliObserver;

struct CancellingObserver {
    inner: CliObserver,
    success_count: Arc<AtomicUsize>,
    threshold: usize,
    cancel: CancellationToken,
}

impl ExecutionObserver for CancellingObserver {
    fn on_event(&self, event: &ExecutionEvent) {
        self.inner.on_event(event);
        if matches!(event, ExecutionEvent::EffectSucceeded { .. }) {
            let successes = self.success_count.fetch_add(1, Ordering::SeqCst) + 1;
            if successes == self.threshold {
                self.cancel.cancel();
            }
        }
    }
}

pub(super) struct ApplyCancellationFixture {
    _dir: TempDir,
    config_path: PathBuf,
    crn_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    backend: LocalBackend,
    provider_context: ProviderContext,
    cancel: CancellationToken,
    cancel_after_successes: Option<usize>,
    success_count: Arc<AtomicUsize>,
}

impl ApplyCancellationFixture {
    pub(super) fn new() -> Self {
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
            provider_context: ProviderContext::default(),
            cancel: CancellationToken::new(),
            cancel_after_successes: None,
            success_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(super) fn with_resources<const N: usize>(self, names: [&str; N]) -> Self {
        let state_path = self.state_path.display();
        let mut crn = format!(
            "backend local {{ path = \"{state_path}\" }}\n\
             provider mock {{}}\n"
        );
        for name in names {
            crn.push_str(&format!(
                "let {name} = mock.test.resource {{ name = \"{name}\" }}\n"
            ));
        }
        std::fs::write(&self.crn_path, crn).expect("write fixture config");
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
        self
    }

    pub(super) fn cancel_after_successes(mut self, count: usize) -> Self {
        self.cancel_after_successes = Some(count);
        self
    }

    pub(super) fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub(super) async fn read_state(&self) -> carina_state::StateFile {
        self.backend
            .read_state()
            .await
            .expect("read state")
            .expect("state exists")
            .into_state()
    }

    pub(super) fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub(super) fn provider_context(&self) -> &ProviderContext {
        &self.provider_context
    }

    pub(super) fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub(super) fn backend(&self) -> &LocalBackend {
        &self.backend
    }

    pub(super) fn observer_factory(&self) -> impl Fn(&Plan) -> Box<dyn ExecutionObserver> + '_ {
        move |plan| {
            if let Some(threshold) = self.cancel_after_successes {
                Box::new(CancellingObserver {
                    inner: CliObserver::new(plan),
                    success_count: Arc::clone(&self.success_count),
                    threshold,
                    cancel: self.cancel.clone(),
                })
            } else {
                Box::new(CliObserver::new(plan))
            }
        }
    }
}
