use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use carina_core::executor::{ExecutionEvent, ExecutionObserver};
use carina_core::parser::ProviderContext;
use carina_core::plan::Plan;
use carina_core::shutdown::ShutdownToken;
use carina_core::shutdown::testing::TestShutdownTrigger;

use crate::commands::shared::cancellation_test_support::{
    CancellationFixtureBase, LockLossBackend, ScopedEnv,
};
use crate::commands::shared::observer::CliObserver;

struct CancellingObserver<S> {
    inner: CliObserver<S>,
    success_count: Arc<AtomicUsize>,
    threshold: usize,
    shutdown_trigger: TestShutdownTrigger,
}

impl<S: Send + Sync + 'static> ExecutionObserver<S> for CancellingObserver<S> {
    fn on_event(&self, event: &ExecutionEvent<S>) {
        self.inner.on_event(event);
        if matches!(event, ExecutionEvent::EffectInterrupted { .. }) {
            // The inner CLI observer renders the unknown outcome. Interrupted
            // effects do not advance this fixture's success threshold.
            return;
        }
        if matches!(event, ExecutionEvent::EffectSucceeded { .. }) {
            let successes = self.success_count.fetch_add(1, Ordering::SeqCst) + 1;
            if successes == self.threshold {
                self.shutdown_trigger.request_graceful_shutdown();
            }
        }
    }
}

pub(super) struct ApplyCancellationFixture {
    base: CancellationFixtureBase,
    provider_context: ProviderContext,
    cancel_after_successes: Option<usize>,
    success_count: Arc<AtomicUsize>,
    provider_ready_path: Option<std::path::PathBuf>,
    provider_release_path: Option<std::path::PathBuf>,
    _provider_env: Vec<ScopedEnv>,
}

impl ApplyCancellationFixture {
    pub(super) fn new() -> Self {
        Self {
            base: CancellationFixtureBase::new(),
            provider_context: ProviderContext::default(),
            cancel_after_successes: None,
            success_count: Arc::new(AtomicUsize::new(0)),
            provider_ready_path: None,
            provider_release_path: None,
            _provider_env: Vec::new(),
        }
    }

    pub(super) fn with_resources<const N: usize>(self, names: [&str; N]) -> Self {
        self.with_resources_and_exports(names, &[])
    }

    pub(super) fn with_resources_and_exports<const N: usize>(
        self,
        names: [&str; N],
        exports: &[(&str, &str)],
    ) -> Self {
        let state_path = self.base.state_path().display();
        let mut crn = format!(
            "backend local {{ path = \"{state_path}\" }}\n\
             provider mock {{}}\n"
        );
        for name in names {
            crn.push_str(&format!(
                "let {name} = mock.test.resource {{ name = \"{name}\" }}\n"
            ));
        }
        if !exports.is_empty() {
            crn.push_str("exports {\n");
            for (name, expr) in exports {
                crn.push_str(&format!("  {name}: String = {expr}\n"));
            }
            crn.push_str("}\n");
        }
        self.base.write_crn(crn);
        self.base.write_backend_lock();
        self
    }

    pub(super) fn with_raw_config(self, crn: impl AsRef<str>) -> Self {
        self.base.write_crn(crn);
        self.base.write_backend_lock();
        self
    }

    pub(super) fn write_state(&self, state: &carina_state::StateFile) {
        self.base.write_state(state);
    }

    pub(super) fn state_path(&self) -> &std::path::Path {
        self.base.state_path()
    }

    pub(super) fn cancel_after_successes(mut self, count: usize) -> Self {
        self.cancel_after_successes = Some(count);
        self
    }

    pub(super) fn with_blocked_create(mut self, name: &str) -> Self {
        let ready_path = self.base.readiness_path("mock-create-ready");
        let release_path = self.base.readiness_path("mock-create-release");
        self._provider_env = vec![
            ScopedEnv::set(
                "CARINA_MOCK_CREATE_DELAY_MS_FOR",
                format!("test.resource.{name}"),
            ),
            ScopedEnv::set("CARINA_MOCK_CREATE_DELAY_MS", "60000"),
            ScopedEnv::set("CARINA_MOCK_CREATE_READY_PATH", &ready_path),
            ScopedEnv::set("CARINA_MOCK_CREATE_RELEASE_PATH", &release_path),
        ];
        self.provider_ready_path = Some(ready_path);
        self.provider_release_path = Some(release_path);
        self
    }

    pub(super) fn release_blocked_create(&self) {
        std::fs::write(
            self.provider_release_path
                .as_ref()
                .expect("blocked create must be configured"),
            b"",
        )
        .expect("write the mock create release handshake");
    }

    pub(super) async fn wait_for_blocked_create(&self) {
        let ready_path = self
            .provider_ready_path
            .as_ref()
            .expect("blocked create must be configured");
        // wall-clock-ok: hang guard waiting for the mock provider readiness file on disk.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("mock provider did not signal that the blocked create started");
    }

    pub(super) fn prioritize_cleanup(&self) {
        self.base.shutdown_trigger().prioritize_cleanup();
    }

    pub(super) fn cancel_token(&self) -> ShutdownToken {
        self.base.shutdown_token()
    }

    pub(super) async fn read_state(&self) -> carina_state::StateFile {
        self.base.read_state().await
    }

    pub(super) fn lock_path(&self) -> &std::path::Path {
        self.base.lock_path()
    }

    pub(super) fn provider_context(&self) -> &ProviderContext {
        &self.provider_context
    }

    pub(super) fn config_path(&self) -> &std::path::Path {
        self.base.config_path()
    }

    pub(super) fn backend_handle(&self) -> Arc<dyn carina_state::StateBackend> {
        self.base.backend_handle()
    }

    pub(super) fn lock_loss_backend_handle(&self) -> Arc<LockLossBackend> {
        self.base.lock_loss_backend_handle()
    }

    pub(super) fn observer_factory<S: Send + Sync + 'static>(
        &self,
    ) -> impl Fn(&Plan<S>, &std::path::Path) -> Box<dyn ExecutionObserver<S>> + '_ {
        move |plan, project_dir| {
            if let Some(threshold) = self.cancel_after_successes {
                Box::new(CancellingObserver {
                    inner: CliObserver::new(plan, project_dir),
                    success_count: Arc::clone(&self.success_count),
                    threshold,
                    shutdown_trigger: self.base.shutdown_trigger(),
                })
            } else {
                Box::new(CliObserver::new(plan, project_dir))
            }
        }
    }
}
