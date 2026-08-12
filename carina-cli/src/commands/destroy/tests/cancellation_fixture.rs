use carina_core::parser::ProviderContext;
use carina_core::shutdown::ShutdownToken;
use carina_state::{LocalBackend, ResourceState, StateFile};

use crate::commands::shared::cancellation_test_support::{CancellationFixtureBase, ScopedEnv};

pub(super) struct DestroyCancellationFixture {
    base: CancellationFixtureBase,
    provider_context: ProviderContext,
    ready_result_barrier: Option<crate::commands::destroy::DestroyReadyResultBarrier>,
    provider_ready_path: Option<std::path::PathBuf>,
    _provider_env: Vec<ScopedEnv>,
}

impl DestroyCancellationFixture {
    pub(super) fn new() -> Self {
        crate::commands::destroy::clear_destroy_success_cancel_after();
        crate::commands::destroy::clear_destroy_ready_result_barrier();

        Self {
            base: CancellationFixtureBase::new(),
            provider_context: ProviderContext::default(),
            ready_result_barrier: None,
            provider_ready_path: None,
            _provider_env: Vec::new(),
        }
    }

    pub(super) fn with_existing_resources<const N: usize>(self, names: [&str; N]) -> Self {
        let state_path = self.base.state_path().display();
        let mut crn = format!(
            "backend local {{ path = \"{state_path}\" }}\n\
             provider mock {{}}\n"
        );
        let mut state = StateFile::new();
        state
            .exports
            .insert("surviving_export".to_string(), serde_json::json!("value"));

        for name in names.iter().rev() {
            crn.push_str(&format!(
                "let {name} = mock.test.resource {{ name = \"{name}\" }}\n"
            ));
        }

        for name in names {
            let mut resource =
                ResourceState::new("test.resource", name, "mock").with_identifier("mock-id");
            resource.binding = Some(name.to_string());
            resource
                .attributes
                .insert("name".to_string(), serde_json::json!(name));
            state
                .upsert_resource(resource)
                .expect("test state setup must be valid");
        }

        self.base.write_crn(crn);
        self.base.write_backend_lock();
        self.base.write_state(&state);

        self
    }

    /// Cancel the token after `count` successful deletions.
    ///
    /// IMPORTANT: this hook fires from inside the in-flight completion handler.
    /// With parallelism > 1, multiple completions can happen between the threshold
    /// being reached and the next dispatch site observing cancel; the test must
    /// therefore use `NonZeroUsize::new(1).unwrap()` parallelism to be deterministic.
    pub(super) fn cancel_after_successes(self, count: usize) -> Self {
        crate::commands::destroy::set_destroy_success_cancel_after(
            count,
            self.base.shutdown_trigger(),
        );
        self
    }

    pub(super) fn with_blocked_delete(mut self, name: &str) -> Self {
        let ready_path = self.base.readiness_path("mock-delete-ready");
        self._provider_env = vec![
            ScopedEnv::set(
                "CARINA_MOCK_DELETE_DELAY_MS_FOR",
                format!("test.resource.{name}"),
            ),
            ScopedEnv::set("CARINA_MOCK_DELETE_DELAY_MS", "60000"),
            ScopedEnv::set("CARINA_MOCK_DELETE_READY_PATH", &ready_path),
        ];
        self.provider_ready_path = Some(ready_path);
        self
    }

    pub(super) async fn wait_for_blocked_delete(&self) {
        let ready_path = self
            .provider_ready_path
            .as_ref()
            .expect("blocked delete must be configured");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("mock provider did not signal that the blocked delete started");
    }

    pub(super) fn prioritize_cleanup(&self) {
        self.base.shutdown_trigger().prioritize_cleanup();
    }

    pub(super) fn with_delete_result_barrier(mut self) -> Self {
        self.ready_result_barrier =
            Some(crate::commands::destroy::install_destroy_ready_result_barrier());
        self
    }

    pub(super) async fn wait_for_delete_result(&self) {
        self.ready_result_barrier
            .as_ref()
            .expect("delete result barrier must be installed")
            .reached()
            .await;
    }

    pub(super) fn release_delete_result_and_prioritize_cleanup(&self) {
        self.ready_result_barrier
            .as_ref()
            .expect("delete result barrier must be installed")
            .release();
        self.base.shutdown_trigger().prioritize_cleanup();
    }

    pub(super) fn cancel_token(&self) -> ShutdownToken {
        self.base.shutdown_token()
    }

    pub(super) async fn read_state(&self) -> StateFile {
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

    pub(super) fn backend(&self) -> &LocalBackend {
        self.base.backend()
    }
}

impl Drop for DestroyCancellationFixture {
    fn drop(&mut self) {
        crate::commands::destroy::clear_destroy_success_cancel_after();
        crate::commands::destroy::clear_destroy_ready_result_barrier();
    }
}
