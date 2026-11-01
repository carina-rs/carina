use carina_core::parser::ProviderContext;
use carina_core::shutdown::ShutdownToken;
use carina_provider_mock::{MockScope, SelectedDelay};
use carina_state::{ResourceState, StateFile};
use std::sync::Arc;
use std::time::Duration;

use crate::commands::shared::cancellation_test_support::{
    CancellationFixtureBase, LockLossBackend, RefreshDrainBarrier, install_refresh_drain_barrier,
};

pub(super) struct DestroyCancellationFixture {
    base: CancellationFixtureBase,
    provider_context: ProviderContext,
    ready_result_barrier: Option<crate::commands::destroy::DestroyReadyResultBarrier>,
    refresh_drain_barrier: Option<RefreshDrainBarrier>,
    provider_ready_path: Option<std::path::PathBuf>,
    provider_read_ready_path: Option<std::path::PathBuf>,
    provider_read_outcome_path: Option<std::path::PathBuf>,
}

impl DestroyCancellationFixture {
    pub(super) fn new() -> Self {
        Self {
            base: CancellationFixtureBase::new(),
            provider_context: ProviderContext::default(),
            ready_result_barrier: None,
            refresh_drain_barrier: None,
            provider_ready_path: None,
            provider_read_ready_path: None,
            provider_read_outcome_path: None,
        }
    }

    pub(super) fn runtime(&self) -> &crate::runtime::CommandRuntime {
        self.base.runtime()
    }
    pub(super) fn with_existing_resources<const N: usize>(self, names: [&str; N]) -> Self {
        let state_path = self.base.state_path().display();
        let mut crn = format!(
            "backend local {{ path = \"{state_path}\" }}\n\
             provider mock {{}}\n"
        );
        let mut state = StateFile::new();
        state.insert_export_json_for_test("surviving_export", serde_json::json!("value"));

        for name in names.iter().rev() {
            crn.push_str(&format!(
                "let {name} = mock.test.resource {{ name = \"{name}\" }}\n"
            ));
        }

        for name in names {
            let identity = carina_core::resource::ResourceIdentity::try_from(name.to_owned())
                .expect("test identity is non-empty");
            let mut resource =
                ResourceState::try_applied("test.resource", identity, "mock", "mock-id")
                    .expect("fixture identifier is non-empty");
            resource.binding = Some(carina_core::resource::absolute_binding_address_for_test(
                name,
            ));
            resource.set_persisted_attribute("name".to_string(), serde_json::json!(name));
            state
                .upsert_resource(resource)
                .expect("test state setup must be valid");
        }

        self.base.write_crn(crn);
        self.base.write_backend_lock();
        self.base.write_state(&state);

        self
    }

    pub(super) fn with_orphaned_resources<const N: usize>(self, names: [&str; N]) -> Self {
        let state_path = self.base.state_path().display();
        self.base.write_crn(format!(
            "backend local {{ path = \"{state_path}\" }}\nprovider mock {{}}\n"
        ));
        let mut state = StateFile::new();
        state.insert_export_json_for_test("surviving_export", serde_json::json!("value"));

        for name in names {
            let identity = carina_core::resource::ResourceIdentity::try_from(name.to_owned())
                .expect("test identity is non-empty");
            let mut resource =
                ResourceState::try_applied("test.resource", identity, "mock", "mock-id")
                    .expect("fixture identifier is non-empty");
            resource.binding = Some(carina_core::resource::absolute_binding_address_for_test(
                name,
            ));
            resource.set_persisted_attribute("name".to_string(), serde_json::json!(name));
            state
                .upsert_resource(resource)
                .expect("test state setup must be valid");
        }

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
            self.runtime(),
            count,
            self.base.shutdown_trigger(),
            self.base.shutdown_token(),
        );
        self
    }

    pub(super) fn with_blocked_delete(mut self, name: &str) -> Self {
        let ready_path = self.base.readiness_path("mock-delete-ready");
        self.base.update_mock(|behavior, _| {
            behavior.delete_delay = Some(SelectedDelay {
                address: format!("test.resource.{name}"),
                duration: Duration::from_secs(60),
            });
            behavior.observations.delete_ready = Some(ready_path.clone());
        });
        self.provider_ready_path = Some(ready_path);
        self
    }

    pub(super) fn with_delayed_delete(mut self, name: &str, delay: Duration) -> Self {
        self.base.update_mock(|behavior, _| {
            behavior.delete_delay = Some(SelectedDelay {
                address: format!("test.resource.{name}"),
                duration: delay,
            });
        });
        self
    }

    pub(super) fn with_blocked_read(mut self, name: &str) -> Self {
        let ready_path = self.base.readiness_path("mock-read-ready");
        let outcome_path = self.base.readiness_path("mock-read-outcome");
        let provider_state_path = self.base.readiness_path("mock-provider-state.json");
        let provider_state = std::collections::HashMap::from([(
            format!("test.resource.{name}"),
            serde_json::json!({ "name": name }),
        )]);
        std::fs::write(
            &provider_state_path,
            carina_core::utils::pretty_with_newline(&provider_state)
                .expect("serialize mock provider state"),
        )
        .expect("write mock provider state");
        self.base.update_mock(|behavior, scope| {
            behavior.read_delay = Some(SelectedDelay {
                address: format!("test.resource.{name}"),
                duration: Duration::from_secs(60),
            });
            behavior.observations.read_ready = Some(ready_path.clone());
            behavior.observations.read_outcome = Some(outcome_path.clone());
            *scope = MockScope::file(provider_state_path).expect("absolute fixture state path");
        });
        self.provider_read_ready_path = Some(ready_path);
        self.provider_read_outcome_path = Some(outcome_path);
        self
    }

    pub(super) fn with_refresh_drain_barrier(mut self) -> Self {
        self.refresh_drain_barrier = Some(install_refresh_drain_barrier(
            self.runtime(),
            self.base.shutdown_token(),
        ));
        self
    }

    pub(super) async fn wait_for_blocked_read(&self) {
        let ready_path = self
            .provider_read_ready_path
            .as_ref()
            .expect("blocked read must be configured");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("mock provider did not signal that the blocked read started");
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

    pub(super) fn request_graceful_shutdown(&self) {
        self.base.shutdown_trigger().request_graceful_shutdown();
    }

    pub(super) async fn wait_for_refresh_drain(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.refresh_drain_barrier
                .as_ref()
                .expect("refresh drain barrier must be installed")
                .reached(),
        )
        .await
        .expect("refresh loop did not enter its in-flight drain wait");
    }

    pub(super) fn prioritize_cleanup_and_release_refresh_drain(&self) {
        self.base.shutdown_trigger().prioritize_cleanup();
        self.refresh_drain_barrier
            .as_ref()
            .expect("refresh drain barrier must be installed")
            .release();
    }

    pub(super) fn read_outcome(&self) -> String {
        std::fs::read_to_string(
            self.provider_read_outcome_path
                .as_ref()
                .expect("blocked read must be configured"),
        )
        .expect("mock provider must record whether the in-flight read completed or was abandoned")
    }

    pub(super) fn with_delete_result_barrier(mut self) -> Self {
        self.ready_result_barrier = Some(
            crate::commands::destroy::install_destroy_ready_result_barrier(
                self.runtime(),
                self.base.shutdown_token(),
            ),
        );
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

    pub(super) fn state_path(&self) -> &std::path::Path {
        self.base.state_path()
    }

    pub(super) fn provider_context(&self) -> &ProviderContext {
        &self.provider_context
    }

    pub(super) fn config_path(&self) -> &std::path::Path {
        self.base.config_path()
    }

    pub(super) fn lock_loss_backend_handle(&self) -> Arc<LockLossBackend> {
        self.base.lock_loss_backend_handle()
    }
}
