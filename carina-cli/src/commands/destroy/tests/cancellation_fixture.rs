use carina_core::parser::ProviderContext;
use carina_state::{LocalBackend, ResourceState, StateFile};
use tokio_util::sync::CancellationToken;

use crate::commands::shared::cancellation_test_support::CancellationFixtureBase;

pub(super) struct DestroyCancellationFixture {
    base: CancellationFixtureBase,
    provider_context: ProviderContext,
}

impl DestroyCancellationFixture {
    pub(super) fn new() -> Self {
        crate::commands::destroy::clear_destroy_success_cancel_after();

        Self {
            base: CancellationFixtureBase::new(),
            provider_context: ProviderContext::default(),
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
            state.upsert_resource(resource);
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
        crate::commands::destroy::set_destroy_success_cancel_after(count, self.base.cancel_token());
        self
    }

    pub(super) fn cancel_token(&self) -> CancellationToken {
        self.base.cancel_token()
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
    }
}
