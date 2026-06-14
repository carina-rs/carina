use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use carina_core::executor::{ExecutionEvent, ExecutionObserver};
use carina_core::parser::ProviderContext;
use carina_core::plan::Plan;
use carina_state::LocalBackend;
use tokio_util::sync::CancellationToken;

use crate::commands::shared::cancellation_test_support::CancellationFixtureBase;
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
    base: CancellationFixtureBase,
    provider_context: ProviderContext,
    cancel_after_successes: Option<usize>,
    success_count: Arc<AtomicUsize>,
}

impl ApplyCancellationFixture {
    pub(super) fn new() -> Self {
        Self {
            base: CancellationFixtureBase::new(),
            provider_context: ProviderContext::default(),
            cancel_after_successes: None,
            success_count: Arc::new(AtomicUsize::new(0)),
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

    pub(super) fn cancel_after_successes(mut self, count: usize) -> Self {
        self.cancel_after_successes = Some(count);
        self
    }

    pub(super) fn cancel_token(&self) -> CancellationToken {
        self.base.cancel_token()
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

    pub(super) fn backend(&self) -> &LocalBackend {
        self.base.backend()
    }

    pub(super) fn observer_factory(&self) -> impl Fn(&Plan) -> Box<dyn ExecutionObserver> + '_ {
        move |plan| {
            if let Some(threshold) = self.cancel_after_successes {
                Box::new(CancellingObserver {
                    inner: CliObserver::new(plan),
                    success_count: Arc::clone(&self.success_count),
                    threshold,
                    cancel: self.base.cancel_token(),
                })
            } else {
                Box::new(CliObserver::new(plan))
            }
        }
    }
}
