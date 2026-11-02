//! All mutable command instrumentation belongs to one invocation.
use crate::cursor::CursorState;
use crate::error::{AppError, CommandInputError};
use crate::output_style::ColorMode;
use crate::wiring::TrustedLocalGrantInputs;
use carina_core::UserDirs;
use carina_provider_mock::ConfiguredBuiltinMock;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "test-utils"))]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

type ConfigureBuiltinMock =
    Box<dyn FnOnce() -> Result<ConfiguredBuiltinMock, Arc<CommandInputError>> + Send + Sync>;

/// Mock-only inputs become usable only after successful builtin selection.
pub struct BuiltinMockInputs {
    configured:
        LazyLock<Result<ConfiguredBuiltinMock, Arc<CommandInputError>>, ConfigureBuiltinMock>,
}

impl BuiltinMockInputs {
    pub fn deferred(
        configure: impl FnOnce() -> Result<ConfiguredBuiltinMock, CommandInputError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            configured: LazyLock::new(Box::new(|| configure().map_err(Arc::new))),
        }
    }

    fn resolve(&self) -> Result<&ConfiguredBuiltinMock, AppError> {
        self.configured
            .as_ref()
            .map_err(|error| error.clone().into())
    }
}

#[derive(Clone, Default)]
pub struct InvocationHooks {
    pub plan_drift_handshake_dir: Option<PathBuf>,
    pub capture_live_apply_plan: Option<PathBuf>,
    #[cfg(test)]
    test: Arc<TestInvocationHooks>,
}

pub struct CommandInputs {
    pub builtin_mock: BuiltinMockInputs,
    pub trusted_local_grants: TrustedLocalGrantInputs,
    pub accepted_egress: carina_provider_resolver::AcceptedEgressSources,
    pub color_mode: ColorMode,
    pub cursor: CursorState,
    pub hooks: InvocationHooks,
}

struct InvocationState {
    inputs: CommandInputs,
    warned_upstream: Mutex<HashSet<PathBuf>>,
    #[cfg(any(test, feature = "test-utils"))]
    provider_loads: AtomicUsize,
}

#[derive(Clone)]
pub struct CommandRuntime {
    user_dirs: UserDirs,
    state: Arc<InvocationState>,
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) directory_owner: Option<Arc<carina_core::TestUserDirs>>,
}

impl CommandRuntime {
    pub fn new(user_dirs: UserDirs, inputs: CommandInputs) -> Self {
        Self {
            user_dirs,
            state: Arc::new(InvocationState {
                inputs,
                warned_upstream: Mutex::new(HashSet::new()),
                #[cfg(any(test, feature = "test-utils"))]
                provider_loads: AtomicUsize::new(0),
            }),
            #[cfg(any(test, feature = "test-utils"))]
            directory_owner: None,
        }
    }
    pub fn user_dirs(&self) -> &UserDirs {
        &self.user_dirs
    }
    pub(crate) fn builtin_mock(&self) -> Result<&ConfiguredBuiltinMock, AppError> {
        self.state.inputs.builtin_mock.resolve()
    }
    pub(crate) fn grants(&self) -> &TrustedLocalGrantInputs {
        &self.state.inputs.trusted_local_grants
    }
    pub(crate) fn accepted_egress(&self) -> &carina_provider_resolver::AcceptedEgressSources {
        &self.state.inputs.accepted_egress
    }
    pub fn color_mode(&self) -> ColorMode {
        self.state.inputs.color_mode
    }
    pub fn cursor(&self) -> &CursorState {
        &self.state.inputs.cursor
    }
    pub(crate) fn hooks(&self) -> &InvocationHooks {
        &self.state.inputs.hooks
    }
    pub(crate) fn first_upstream_warning(&self, path: &Path) -> bool {
        self.state
            .warned_upstream
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_owned())
    }
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn record_provider_load(&self) {
        self.state.provider_loads.fetch_add(1, Ordering::SeqCst);
    }
    #[cfg(any(test, feature = "test-utils"))]
    pub fn provider_load_count(&self) -> usize {
        self.state.provider_loads.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    pub(crate) fn test_hooks(&self) -> &TestInvocationHooks {
        &self.state.inputs.hooks.test
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestInvocationHooks {
    pub refresh_drain:
        Mutex<Option<crate::commands::shared::cancellation_test_support::RefreshDrainBarrier>>,
    pub destroy_success: Mutex<Option<crate::commands::destroy::DestroySuccessCancelHook>>,
    pub destroy_ready: Mutex<Option<crate::commands::destroy::DestroyReadyResultBarrier>>,
}

impl std::fmt::Debug for CommandRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandRuntime")
            .field("user_dirs", &self.user_dirs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::destroy::{
        install_destroy_ready_result_barrier, observe_destroy_result_ready_for_tests,
        observe_destroy_success_for_tests, set_destroy_success_cancel_after,
    };
    use crate::commands::shared::cancellation_test_support::{
        install_refresh_drain_barrier, observe_refresh_drain_wait,
    };
    use carina_core::shutdown::testing::shutdown_channel;

    #[test]
    fn reconfiguring_mock_after_runtime_clone_builds_a_new_invocation() {
        let mut fixture = crate::test_support::TestCommandRuntime::new();
        let shared = CommandRuntime::clone(&fixture);
        shared.record_provider_load();
        let source = Path::new("/same/canonical/upstream");
        assert!(shared.first_upstream_warning(source));

        fixture.set_mock(
            carina_provider_mock::MockBehavior::default(),
            carina_provider_mock::MockScope::memory(),
        );

        assert_eq!(fixture.provider_load_count(), 0);
        assert_eq!(shared.provider_load_count(), 1);
        assert!(fixture.first_upstream_warning(source));
        assert!(!shared.first_upstream_warning(source));
        assert_eq!(fixture.user_dirs(), shared.user_dirs());
    }

    async fn assert_reconfiguration_preserves_installed_hooks(
        reconfigure: impl FnOnce(&mut crate::test_support::TestCommandRuntime),
    ) {
        tokio::time::pause();
        let mut fixture = crate::test_support::TestCommandRuntime::new();
        let (trigger, shutdown) = shutdown_channel();
        let refresh = install_refresh_drain_barrier(&fixture, shutdown.clone());
        let ready = install_destroy_ready_result_barrier(&fixture, shutdown.clone());
        set_destroy_success_cancel_after(&fixture, 1, trigger, shutdown.clone());

        reconfigure(&mut fixture);

        let id = carina_core::resource::ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "same",
            None,
        );
        observe_destroy_success_for_tests(&fixture, 1, &id, &shutdown);
        assert_ne!(
            shutdown.phase(),
            carina_core::shutdown::ShutdownPhase::Running,
            "destroy success hook must survive mock reconfiguration"
        );

        let exercise = async {
            let waiting = async {
                tokio::join!(
                    observe_refresh_drain_wait(&fixture, &shutdown),
                    observe_destroy_result_ready_for_tests(&fixture, &shutdown)
                );
            };
            let release = async {
                tokio::join!(refresh.reached(), ready.reached());
                refresh.release();
                ready.release();
            };
            tokio::join!(waiting, release);
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), exercise)
            .await
            .expect("refresh drain and destroy ready hooks must survive mock reconfiguration");
    }

    #[tokio::test]
    async fn set_mock_preserves_installed_hooks() {
        assert_reconfiguration_preserves_installed_hooks(|fixture| {
            fixture.set_mock(
                carina_provider_mock::MockBehavior::default(),
                carina_provider_mock::MockScope::memory(),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn update_mock_preserves_installed_hooks() {
        assert_reconfiguration_preserves_installed_hooks(|fixture| {
            fixture.update_mock(|behavior, scope| {
                *behavior = carina_provider_mock::MockBehavior::default();
                *scope = carina_provider_mock::MockScope::memory();
            });
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_invocation_hook_sets_do_not_overwrite_each_other() {
        let a = crate::test_support::TestCommandRuntime::new();
        let b = crate::test_support::TestCommandRuntime::new();
        let (a_trigger, a_shutdown) = shutdown_channel();
        let (b_trigger, b_shutdown) = shutdown_channel();
        let a_refresh = install_refresh_drain_barrier(&a, a_shutdown.clone());
        let b_refresh = install_refresh_drain_barrier(&b, b_shutdown.clone());
        let a_ready = install_destroy_ready_result_barrier(&a, a_shutdown.clone());
        let b_ready = install_destroy_ready_result_barrier(&b, b_shutdown.clone());
        set_destroy_success_cancel_after(&a, 1, a_trigger, a_shutdown.clone());
        set_destroy_success_cancel_after(&b, 2, b_trigger, b_shutdown.clone());
        let id = carina_core::resource::ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "same",
            None,
        );
        observe_destroy_success_for_tests(&a, 1, &id, &a_shutdown);
        assert_ne!(
            a_shutdown.phase(),
            carina_core::shutdown::ShutdownPhase::Running
        );
        assert_eq!(
            b_shutdown.phase(),
            carina_core::shutdown::ShutdownPhase::Running
        );
        observe_destroy_success_for_tests(&b, 2, &id, &b_shutdown);
        assert_ne!(
            b_shutdown.phase(),
            carina_core::shutdown::ShutdownPhase::Running
        );
        let exercise = async {
            let waiting = async {
                tokio::join!(
                    observe_refresh_drain_wait(&a, &a_shutdown),
                    observe_refresh_drain_wait(&b, &b_shutdown),
                    observe_destroy_result_ready_for_tests(&a, &a_shutdown),
                    observe_destroy_result_ready_for_tests(&b, &b_shutdown)
                );
            };
            let release = async {
                tokio::join!(
                    a_refresh.reached(),
                    b_refresh.reached(),
                    a_ready.reached(),
                    b_ready.reached()
                );
                a_refresh.release();
                b_refresh.release();
                a_ready.release();
                b_ready.release();
            };
            tokio::join!(waiting, release);
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), exercise)
            .await
            .expect("both installed hook sets must be reached");
    }

    #[test]
    fn upstream_warning_deduplication_is_per_invocation_across_threads() {
        let a = crate::test_support::TestCommandRuntime::new();
        let b = crate::test_support::TestCommandRuntime::new();
        let source = Path::new("/same/canonical/upstream");
        assert!(a.first_upstream_warning(source));
        assert!(!a.first_upstream_warning(source));
        std::thread::scope(|threads| {
            threads
                .spawn(|| {
                    assert!(!a.first_upstream_warning(source));
                    assert!(b.first_upstream_warning(source));
                })
                .join()
                .unwrap();
        });
        assert!(!b.first_upstream_warning(source));
    }
}
