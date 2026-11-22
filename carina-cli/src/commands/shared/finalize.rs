use crate::error::AppError;
use crate::output_style::Colorize;
use carina_core::hint::ProjectCommand;
use carina_core::shutdown::{CleanupBudget, ShutdownPhase, ShutdownToken, duration_for_log};
use carina_state::{PublishedBackend, SaveFailureKind};
use std::future::Future;
use std::path::Path;

use super::lock_heartbeat::{StateLock, StateLockRef};

/// Project identity carried through state flushing and lock release.
#[derive(Clone, Copy)]
pub(crate) struct FinalizationContext<'a> {
    backend: &'a PublishedBackend,
    project_dir: &'a Path,
}

impl<'a> FinalizationContext<'a> {
    pub(crate) fn new(backend: &'a PublishedBackend, project_dir: &'a Path) -> Self {
        Self {
            backend,
            project_dir,
        }
    }
}

/// Result of a command that consumed its heartbeat outcome.
///
/// Pre-heartbeat failures keep the lock so the command can release it. A
/// heartbeat loss carries only the terminal command error.
pub(crate) enum LockCommandOutcome<T> {
    Kept { result: T, lock: StateLock },
    Lost(AppError),
}

#[derive(Clone, Copy)]
pub(crate) enum StatePersistence<'a> {
    Normal,
    CancellationCleanup(&'a PublishedBackend),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupStageMode {
    Normal,
    CancellationCleanup,
}

impl CleanupStageMode {
    fn at_start(already_cancelled: bool, phase: ShutdownPhase) -> Self {
        match phase {
            ShutdownPhase::Running => {
                if already_cancelled {
                    Self::CancellationCleanup
                } else {
                    Self::Normal
                }
            }
            ShutdownPhase::Graceful => Self::CancellationCleanup,
            ShutdownPhase::CleanupPriority => Self::CancellationCleanup,
        }
    }
}

pub(crate) async fn release_lock_after_execute(
    context: FinalizationContext<'_>,
    lock: StateLockRef<'_>,
) -> Result<(), AppError> {
    let backend = context.backend;
    let shutdown = backend.shutdown();
    let StateLockRef::Held(lock) = lock else {
        return Ok(());
    };
    let release = async {
        match shutdown.phase() {
            ShutdownPhase::CleanupPriority | ShutdownPhase::Graceful => {
                backend.release_lock_for_cleanup(lock).await
            }
            ShutdownPhase::Running => backend.release_lock(lock).await,
        }
    };
    await_cleanup_stage(
        release,
        shutdown,
        false,
        "releasing state lock...",
        "state lock released.",
        "state lock release failed",
    )
    .await
    .map_err(AppError::Backend)
}

/// Finish a lock-owning command, releasing only when ownership survived.
pub(crate) async fn finish_lock_command<T>(
    color_mode: crate::output_style::ColorMode,
    context: FinalizationContext<'_>,
    outcome: LockCommandOutcome<Result<T, AppError>>,
) -> Result<T, AppError> {
    let (result, lock) = match outcome {
        LockCommandOutcome::Kept { result, lock } => (result, lock),
        LockCommandOutcome::Lost(error) => return Err(error),
    };

    let release_result = release_lock_after_execute(context, lock.as_ref()).await;
    if lock.is_held()
        && release_result.is_ok()
        && (result.is_ok() || matches!(result, Err(AppError::Interrupted)))
    {
        println!("  {} Lock released", "✓".green(color_mode));
    }

    let value = result?;
    release_result?;
    Ok(value)
}

/// Handle state finalization after a mutating command has observed execution.
///
/// Four cases:
/// 1. not cancelled + finalize Ok    -> return the finalized value
/// 2. not cancelled + finalize Err   -> Err(finalize_err)
/// 3. cancelled     + finalize Ok    -> Err(Interrupted)
/// 4. cancelled     + finalize Err   -> Err(Interrupted) after logging warning
///
/// On cancellation, `Interrupted` takes precedence so the user sees the
/// interruption instead of a cleanup error. The finalize error is still logged.
pub(crate) async fn finalize_after_execute<'a, T, MakeFinalize, F>(
    context: FinalizationContext<'a>,
    make_finalize: MakeFinalize,
    execution_cancelled: bool,
) -> Result<T, AppError>
where
    MakeFinalize: FnOnce(StatePersistence<'a>) -> F,
    F: Future<Output = Result<T, AppError>>,
{
    let backend = context.backend;
    let shutdown = backend.shutdown();
    let persistence = match shutdown.phase() {
        ShutdownPhase::Running if !execution_cancelled => StatePersistence::Normal,
        ShutdownPhase::Running | ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            StatePersistence::CancellationCleanup(backend)
        }
    };
    let finalize = make_finalize(persistence);
    let Some(finalize_result) =
        await_state_flush_stage(finalize, context, execution_cancelled).await
    else {
        eprintln!(
            "{}",
            state_flush_timeout_message(backend.cleanup_budget(), context.project_dir)
        );
        return Err(AppError::Interrupted);
    };
    match shutdown.phase() {
        ShutdownPhase::Running if !execution_cancelled => finalize_result,
        ShutdownPhase::Running | ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            Err(AppError::Interrupted)
        }
    }
}

fn state_flush_timeout_message(budget: CleanupBudget, project_dir: &Path) -> String {
    let refresh = ProjectCommand::new("state refresh", project_dir);
    format!(
        "Cancellation cleanup: the state save could not be confirmed within {}; continuing to state lock release. Run `{refresh}` to reconcile state with the provider.",
        duration_for_log(budget.state_flush())
    )
}

async fn await_cleanup_stage<T, E, F>(
    future: F,
    shutdown: &ShutdownToken,
    already_cancelled: bool,
    started: &str,
    completed: &str,
    failed: &str,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::pin!(future);
    let result = match CleanupStageMode::at_start(already_cancelled, shutdown.phase()) {
        CleanupStageMode::Normal => {
            tokio::select! {
                result = &mut future => result,
                _ = shutdown.cancelled() => {
                    eprintln!("Cancellation cleanup: {started}");
                    future.await
                }
            }
        }
        CleanupStageMode::CancellationCleanup => {
            eprintln!("Cancellation cleanup: {started}");
            future.await
        }
    };

    match shutdown.phase() {
        ShutdownPhase::Running if !already_cancelled => {}
        ShutdownPhase::Running | ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            eprintln!(
                "{}",
                cleanup_stage_terminal_message(&result, completed, failed)
            );
        }
    }
    result
}

async fn await_state_flush_stage<T, F>(
    future: F,
    context: FinalizationContext<'_>,
    already_cancelled: bool,
) -> Option<Result<T, AppError>>
where
    F: Future<Output = Result<T, AppError>>,
{
    let shutdown = context.backend.shutdown();
    let budget = context.backend.cleanup_budget();
    tokio::pin!(future);
    let cutoff = shutdown.state_flush_budget_expired(budget);
    tokio::pin!(cutoff);
    let mut mode = CleanupStageMode::at_start(already_cancelled, shutdown.phase());
    match mode {
        CleanupStageMode::Normal => {}
        CleanupStageMode::CancellationCleanup => {
            eprintln!("Cancellation cleanup: flushing state...");
        }
    }

    let result = loop {
        tokio::select! {
            biased;
            result = &mut future => break Some(result),
            _ = &mut cutoff => break None,
            _ = shutdown.cancelled(), if matches!(mode, CleanupStageMode::Normal) => {
                mode = CleanupStageMode::CancellationCleanup;
                eprintln!("Cancellation cleanup: flushing state...");
            }
        }
    };

    match shutdown.phase() {
        ShutdownPhase::Running if !already_cancelled => {}
        ShutdownPhase::Running | ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => {
            if let Some(stage_result) = result.as_ref() {
                let failure = match stage_result {
                    Err(AppError::Backend(error)) => Some(error.save_failure_kind()),
                    Err(AppError::ProjectBackend(error)) => {
                        Some(error.backend_error().save_failure_kind())
                    }
                    Ok(_) | Err(_) => None,
                };
                let message = match failure {
                    Some(SaveFailureKind::Unconfirmed) => {
                        state_flush_timeout_message(budget, context.project_dir)
                    }
                    Some(SaveFailureKind::Failed) | None => cleanup_stage_terminal_message(
                        stage_result,
                        "state flushed.",
                        "state flush failed",
                    ),
                };
                eprintln!("{}", message);
            }
        }
    }
    result
}

fn cleanup_stage_terminal_message<T, E: std::fmt::Display>(
    result: &Result<T, E>,
    completed: &str,
    failed: &str,
) -> String {
    match result {
        Ok(_) => format!("Cancellation cleanup: {completed}"),
        Err(err) => format!("Cancellation cleanup: {failed}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::shutdown::CLEANUP_DEADLINE;
    use carina_core::shutdown::testing::shutdown_channel;
    use carina_state::{BackendResult, LockInfo, StateBackend, StateFile};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn cleanup_timeout_hint_preserves_cwd_wording_and_names_a_non_cwd_project() {
        let budget = CleanupBudget::new(Duration::from_secs(5), Duration::from_secs(8));
        let approved = "Cancellation cleanup: the state save could not be confirmed within 5 seconds; continuing to state lock release. Run `carina state refresh` to reconcile state with the provider.";
        assert_eq!(
            state_flush_timeout_message(budget, Path::new(".")),
            approved
        );
        assert_eq!(
            state_flush_timeout_message(budget, Path::new("infra/team project")),
            approved.replace(
                "carina state refresh",
                "carina state refresh 'infra/team project'"
            )
        );
    }

    #[test]
    fn failed_cleanup_stage_has_an_explicit_terminal_log_line() {
        let result: Result<(), &str> = Err("simulated S3 timeout");

        assert_eq!(
            cleanup_stage_terminal_message(
                &result,
                "state lock released.",
                "state lock release failed",
            ),
            "Cancellation cleanup: state lock release failed: simulated S3 timeout"
        );
    }

    #[test]
    fn cleanup_backend_deadline_prints_unconfirmed_save_hint_but_denial_stays_failed() {
        const CHILD: &str = "CARINA_ROUND12_CLEANUP_SAVE_ERROR";
        let Ok(scenario) = std::env::var(CHILD) else {
            for scenario in ["deadline", "transport_deadline", "s3_timeout", "denied"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "commands::shared::finalize::tests::cleanup_backend_deadline_prints_unconfirmed_save_hint_but_denial_stays_failed", "--nocapture"])
                    .env(CHILD, scenario)
                    .output().unwrap();
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    output.status.success(),
                    "{scenario}: {stderr}\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                assert!(
                    stderr.contains("Cancellation cleanup: state lock released."),
                    "{stderr}"
                );
                if scenario == "denied" {
                    assert!(stderr.contains("Cancellation cleanup: state flush failed: GitHub token needs Contents write"), "{stderr}");
                    assert!(
                        !stderr.contains("state save could not be confirmed"),
                        "{stderr}"
                    );
                } else {
                    let flush = if scenario == "s3_timeout" {
                        "1 second"
                    } else {
                        "5 seconds"
                    };
                    assert!(stderr.contains(&format!("Cancellation cleanup: the state save could not be confirmed within {flush}; continuing to state lock release. Run `carina state refresh 'infra/team project'` to reconcile state with the provider.")), "{scenario}: {stderr}");
                    assert!(!stderr.contains("state flush failed"), "{stderr}");
                }
            }
            return;
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let error: carina_state::BackendError = match scenario.as_str() {
                    "deadline" => carina_state::BackendError::UnconfirmedStateSave {
                        error: Box::new(carina_state::GitHubError::MutationDeadline.into()),
                    },
                    "transport_deadline" => carina_state::BackendError::UnconfirmedStateSave {
                        error: Box::new(
                            carina_state::GitHubError::MutationTransportDeadline {
                                error: "offline timeout after dispatch".into(),
                            }
                            .into(),
                        ),
                    },
                    "s3_timeout" => carina_state::BackendError::UnconfirmedStateSave {
                        error: Box::new(carina_state::BackendError::Io(
                            "offline S3 state PUT timeout".into(),
                        )),
                    },
                    "denied" => carina_state::GitHubError::ContentsWriteDenied {
                        target: "owner/repo".into(),
                    }
                    .into(),
                    _ => panic!("unknown cleanup scenario"),
                };
                let latency = if scenario == "s3_timeout" {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(1500)
                };
                let mut backend = LatencyBackend::with_latencies(latency, Duration::ZERO);
                backend.cleanup_budget = if scenario == "s3_timeout" {
                    CleanupBudget::TWO_SECOND
                } else {
                    CleanupBudget::new(Duration::from_secs(5), Duration::from_secs(8))
                };
                *backend.save_error.lock().unwrap() = Some(error);
                let backend = Arc::new(backend);
                let (trigger, shutdown) = shutdown_channel();
                let published = PublishedBackend::for_test(backend.clone(), &shutdown);
                let lock = LockInfo::for_test("apply");
                let mut state = StateFile::new();
                trigger.prioritize_cleanup();
                let start = tokio::time::Instant::now();
                let context = FinalizationContext::new(&published, Path::new("infra/team project"));
                let result = finalize_after_execute(
                    context,
                    |mode| {
                        crate::commands::apply::save_state_locked_after_execute(
                            backend.as_ref(),
                            &lock,
                            &mut state,
                            mode,
                        )
                    },
                    true,
                )
                .await;
                assert!(matches!(result, Err(AppError::Interrupted)));
                assert_eq!(
                    start.elapsed(),
                    latency * 3,
                    "the backend error must precede the CLI cutoff"
                );
                assert!(!backend.state_written.load(Ordering::SeqCst));
                assert_eq!(
                    backend.operations.lock().unwrap().last(),
                    Some(&"cleanup state PUT")
                );
                release_lock_after_execute(context, StateLockRef::Held(&lock))
                    .await
                    .unwrap();
                assert!(!backend.lock_held.load(Ordering::SeqCst));
            });
    }

    #[tokio::test(start_paused = true)]
    async fn slow_bounded_cleanup_operations_finish_inside_the_hard_deadline() {
        let backend = Arc::new(LatencyBackend::new(Duration::from_millis(250)));
        let lock = LockInfo::for_test("apply");
        let mut state = StateFile::new();
        let (trigger, shutdown) = shutdown_channel();
        let published = PublishedBackend::for_test(backend.clone(), &shutdown);
        trigger.prioritize_cleanup();

        tokio::time::timeout(CLEANUP_DEADLINE, async {
            let result = finalize_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                |mode| {
                    crate::commands::apply::save_state_locked_after_execute(
                        backend.as_ref(),
                        &lock,
                        &mut state,
                        mode,
                    )
                },
                true,
            )
            .await;
            assert!(matches!(result, Err(AppError::Interrupted)));
            release_lock_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                StateLockRef::Held(&lock),
            )
            .await
            .unwrap();
        })
        .await
        .expect("five slow-but-bounded cleanup operations must fit inside the 2s deadline");

        assert!(backend.state_written.load(Ordering::SeqCst));
        assert!(!backend.lock_held.load(Ordering::SeqCst));
        assert_eq!(
            *backend.operations.lock().unwrap(),
            [
                "cleanup lock GET",
                "cleanup lock renewal PUT",
                "cleanup state PUT",
                "cleanup release GET",
                "cleanup release DELETE",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_state_flush_cannot_starve_cleanup_lock_release() {
        let backend = Arc::new(LatencyBackend::with_latencies(
            Duration::from_secs(5),
            Duration::from_millis(250),
        ));
        let lock = LockInfo::for_test("destroy");
        let mut state = StateFile::new();
        let (trigger, shutdown) = shutdown_channel();
        let published = PublishedBackend::for_test(backend.clone(), &shutdown);
        trigger.prioritize_cleanup();

        tokio::time::timeout(CLEANUP_DEADLINE, async {
            let result = finalize_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                |mode| {
                    crate::commands::apply::save_state_locked_after_execute(
                        backend.as_ref(),
                        &lock,
                        &mut state,
                        mode,
                    )
                },
                true,
            )
            .await;
            assert!(matches!(result, Err(AppError::Interrupted)));
            release_lock_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                StateLockRef::Held(&lock),
            )
            .await
            .unwrap();
        })
        .await
        .expect("state flush must yield the reserved lock-release slice");

        assert!(!backend.state_written.load(Ordering::SeqCst));
        assert!(!backend.lock_held.load(Ordering::SeqCst));
        assert_eq!(
            *backend.operations.lock().unwrap(),
            [
                "cleanup lock GET",
                "cleanup release GET",
                "cleanup release DELETE",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn finalizer_uses_backend_provided_state_flush_budget() {
        for before_finalize in [
            Duration::ZERO,
            Duration::from_millis(350),
            Duration::from_millis(600),
        ] {
            let mut backend = LatencyBackend::new(Duration::from_secs(5));
            backend.cleanup_budget =
                CleanupBudget::new(Duration::from_millis(400), Duration::from_secs(2));
            let backend = Arc::new(backend);
            let lock = LockInfo::for_test("apply");
            let mut state = StateFile::new();
            let (trigger, shutdown) = shutdown_channel();
            let published = PublishedBackend::for_test(backend.clone(), &shutdown);
            trigger.prioritize_cleanup();
            tokio::time::advance(before_finalize).await;

            let start = tokio::time::Instant::now();
            let result = finalize_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                |mode| {
                    crate::commands::apply::save_state_locked_after_execute(
                        backend.as_ref(),
                        &lock,
                        &mut state,
                        mode,
                    )
                },
                true,
            )
            .await;

            assert!(matches!(result, Err(AppError::Interrupted)));
            assert_eq!(
                start.elapsed(),
                backend
                    .cleanup_budget
                    .state_flush()
                    .saturating_sub(before_finalize)
            );
            assert!(!backend.state_written.load(Ordering::SeqCst));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn github_sized_flush_window_preserves_the_lock_release_slice() {
        for (state_latency, saved, elapsed) in [
            (
                Duration::from_millis(1500),
                true,
                Duration::from_millis(6500),
            ),
            (Duration::from_secs(10), false, Duration::from_secs(7)),
        ] {
            let mut backend = LatencyBackend::with_latencies(state_latency, Duration::from_secs(1));
            backend.cleanup_budget =
                CleanupBudget::new(Duration::from_secs(5), Duration::from_secs(8));
            let backend = Arc::new(backend);
            let (trigger, shutdown) = shutdown_channel();
            let published = PublishedBackend::for_test(backend.clone(), &shutdown);
            let lock = LockInfo::for_test("apply");
            let mut state = StateFile::new();
            trigger.prioritize_cleanup();
            let start = tokio::time::Instant::now();
            let result = finalize_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                |mode| {
                    crate::commands::apply::save_state_locked_after_execute(
                        backend.as_ref(),
                        &lock,
                        &mut state,
                        mode,
                    )
                },
                true,
            )
            .await;
            assert!(matches!(result, Err(AppError::Interrupted)));
            release_lock_after_execute(
                FinalizationContext::new(&published, Path::new(".")),
                StateLockRef::Held(&lock),
            )
            .await
            .unwrap();
            assert_eq!(start.elapsed(), elapsed);
            assert!(start.elapsed() < published.cleanup_budget().total());
            assert_eq!(backend.state_written.load(Ordering::SeqCst), saved);
            assert!(!backend.lock_held.load(Ordering::SeqCst));
        }
    }

    struct LatencyBackend {
        cleanup_budget: carina_core::shutdown::CleanupBudget,
        state_operation_latency: Duration,
        release_operation_latency: Duration,
        operations: Arc<Mutex<Vec<&'static str>>>,
        state_written: AtomicBool,
        lock_held: AtomicBool,
        save_error: Mutex<Option<carina_state::BackendError>>,
    }

    impl LatencyBackend {
        fn new(operation_latency: Duration) -> Self {
            Self::with_latencies(operation_latency, operation_latency)
        }

        fn with_latencies(
            state_operation_latency: Duration,
            release_operation_latency: Duration,
        ) -> Self {
            Self {
                cleanup_budget: carina_core::shutdown::CleanupBudget::TWO_SECOND,
                state_operation_latency,
                release_operation_latency,
                operations: Arc::new(Mutex::new(Vec::new())),
                state_written: AtomicBool::new(false),
                lock_held: AtomicBool::new(true),
                save_error: Mutex::new(None),
            }
        }

        async fn operation(&self, name: &'static str, latency: Duration) {
            self.operations.lock().unwrap().push(name);
            tokio::time::sleep(latency).await;
        }
    }

    #[async_trait::async_trait]
    impl StateBackend for LatencyBackend {
        fn cleanup_budget(&self) -> carina_core::shutdown::CleanupBudget {
            self.cleanup_budget
        }

        async fn read_state(&self) -> BackendResult<Option<StateFile>> {
            Ok(None)
        }

        async fn read_state_bytes(&self) -> BackendResult<Option<carina_state::RawState>> {
            Ok(None)
        }

        async fn write_state(&self, _state: &StateFile) -> BackendResult<()> {
            panic!("normal state write must not serve cancellation cleanup")
        }

        async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
            Ok(LockInfo::for_test(operation))
        }

        async fn release_lock(&self, _lock: &LockInfo) -> BackendResult<()> {
            panic!("normal lock release must not serve cancellation cleanup")
        }

        async fn renew_lock(&self, _lock: &LockInfo) -> BackendResult<LockInfo> {
            panic!("normal lock renewal must not serve cancellation cleanup")
        }

        async fn write_state_locked(
            &self,
            _state: &StateFile,
            _lock: &LockInfo,
        ) -> BackendResult<()> {
            panic!("normal locked write must not serve cancellation cleanup")
        }

        async fn write_state_locked_for_cleanup(
            &self,
            _state: &StateFile,
            lock: &LockInfo,
            _permit: &carina_state::CleanupPermit<'_>,
        ) -> BackendResult<()> {
            self.operation("cleanup lock GET", self.state_operation_latency)
                .await;
            self.operation("cleanup lock renewal PUT", self.state_operation_latency)
                .await;
            self.operation("cleanup state PUT", self.state_operation_latency)
                .await;
            if let Some(error) = self.save_error.lock().unwrap().take() {
                return Err(error);
            }
            self.state_written.store(true, Ordering::SeqCst);
            assert!(self.lock_held.load(Ordering::SeqCst));
            let _ = lock;
            Ok(())
        }

        async fn release_lock_for_cleanup(
            &self,
            _lock: &LockInfo,
            _permit: &carina_state::CleanupPermit<'_>,
        ) -> BackendResult<()> {
            self.operation("cleanup release GET", self.release_operation_latency)
                .await;
            self.operation("cleanup release DELETE", self.release_operation_latency)
                .await;
            self.lock_held.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn force_unlock(&self, _lock_id: &str) -> BackendResult<()> {
            Ok(())
        }

        async fn storage_readiness(
            &self,
        ) -> carina_state::BackendResult<carina_state::StorageReadiness> {
            Ok(carina_state::StorageReadiness::Ready)
        }
    }
}
