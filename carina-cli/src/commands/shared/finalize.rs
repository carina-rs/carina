use crate::error::AppError;
use carina_core::shutdown::{ShutdownPhase, ShutdownToken};
use carina_state::{LockInfo, StateBackend};
use std::future::Future;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatePersistence {
    Normal,
    CancellationCleanup,
}

pub(crate) async fn release_lock_after_execute(
    backend: &dyn StateBackend,
    lock: &LockInfo,
    shutdown: &ShutdownToken,
) -> Result<(), AppError> {
    let release = match shutdown.phase() {
        ShutdownPhase::CleanupPriority | ShutdownPhase::Graceful => {
            backend.release_lock_for_cleanup(lock)
        }
        ShutdownPhase::Running => backend.release_lock(lock),
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
pub(crate) async fn finalize_after_execute<T, MakeFinalize, F>(
    make_finalize: MakeFinalize,
    execution_cancelled: bool,
    shutdown: &ShutdownToken,
) -> Result<T, AppError>
where
    MakeFinalize: FnOnce(StatePersistence) -> F,
    F: Future<Output = Result<T, AppError>>,
{
    let persistence = if execution_cancelled || shutdown_requested(shutdown) {
        StatePersistence::CancellationCleanup
    } else {
        StatePersistence::Normal
    };
    let finalize = make_finalize(persistence);
    let Some(finalize_result) = await_state_flush_stage(
        finalize,
        shutdown,
        execution_cancelled,
        "flushing state...",
        "state flushed.",
        "state flush failed",
    )
    .await
    else {
        eprintln!(
            "Cancellation cleanup: state flush budget expired; continuing to state lock release."
        );
        return Err(AppError::Interrupted);
    };
    let cancelled = execution_cancelled || shutdown_requested(shutdown);

    if cancelled {
        return Err(AppError::Interrupted);
    }

    finalize_result
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
    let logged_start = already_cancelled || shutdown_requested(shutdown);
    if logged_start {
        eprintln!("Cancellation cleanup: {started}");
    }

    let result = if logged_start {
        future.await
    } else {
        tokio::select! {
            result = &mut future => result,
            _ = shutdown.cancelled() => {
                eprintln!("Cancellation cleanup: {started}");
                future.await
            }
        }
    };

    if already_cancelled || shutdown_requested(shutdown) {
        eprintln!(
            "{}",
            cleanup_stage_terminal_message(&result, completed, failed)
        );
    }
    result
}

async fn await_state_flush_stage<T, E, F>(
    future: F,
    shutdown: &ShutdownToken,
    already_cancelled: bool,
    started: &str,
    completed: &str,
    failed: &str,
) -> Option<Result<T, E>>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::pin!(future);
    let mut logged_start = already_cancelled || shutdown_requested(shutdown);
    if logged_start {
        eprintln!("Cancellation cleanup: {started}");
    }

    let result = loop {
        tokio::select! {
            biased;
            result = &mut future => break Some(result),
            _ = shutdown.state_flush_budget_expired() => break None,
            _ = shutdown.cancelled(), if !logged_start => {
                logged_start = true;
                eprintln!("Cancellation cleanup: {started}");
            }
        }
    };

    if shutdown_requested(shutdown)
        && let Some(stage_result) = result.as_ref()
    {
        eprintln!(
            "{}",
            cleanup_stage_terminal_message(stage_result, completed, failed)
        );
    }
    result
}

fn shutdown_requested(shutdown: &ShutdownToken) -> bool {
    match shutdown.phase() {
        ShutdownPhase::Running => false,
        ShutdownPhase::Graceful | ShutdownPhase::CleanupPriority => true,
    }
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
    use carina_state::{BackendResult, LoadedState, StateFile};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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

    #[tokio::test(start_paused = true)]
    async fn slow_bounded_cleanup_operations_finish_inside_the_hard_deadline() {
        let backend = LatencyBackend::new(Duration::from_millis(250));
        let lock = LockInfo::new("apply");
        let mut state = StateFile::new();
        let (trigger, shutdown) = shutdown_channel();
        trigger.prioritize_cleanup();
        schedule_state_flush_cutoff(trigger.clone());

        tokio::time::timeout(CLEANUP_DEADLINE, async {
            let result = finalize_after_execute(
                |mode| {
                    crate::commands::apply::save_state_locked_after_execute(
                        &backend, &lock, &mut state, mode,
                    )
                },
                true,
                &shutdown,
            )
            .await;
            assert!(matches!(result, Err(AppError::Interrupted)));
            release_lock_after_execute(&backend, &lock, &shutdown)
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
        let backend =
            LatencyBackend::with_latencies(Duration::from_secs(5), Duration::from_millis(250));
        let lock = LockInfo::new("destroy");
        let mut state = StateFile::new();
        let (trigger, shutdown) = shutdown_channel();
        trigger.prioritize_cleanup();
        schedule_state_flush_cutoff(trigger.clone());

        tokio::time::timeout(CLEANUP_DEADLINE, async {
            let result = finalize_after_execute(
                |mode| {
                    crate::commands::apply::save_state_locked_after_execute(
                        &backend, &lock, &mut state, mode,
                    )
                },
                true,
                &shutdown,
            )
            .await;
            assert!(matches!(result, Err(AppError::Interrupted)));
            release_lock_after_execute(&backend, &lock, &shutdown)
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

    fn schedule_state_flush_cutoff(trigger: carina_core::shutdown::testing::TestShutdownTrigger) {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            trigger.expire_state_flush_budget();
        });
    }

    struct LatencyBackend {
        state_operation_latency: Duration,
        release_operation_latency: Duration,
        operations: Arc<Mutex<Vec<&'static str>>>,
        state_written: AtomicBool,
        lock_held: AtomicBool,
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
                state_operation_latency,
                release_operation_latency,
                operations: Arc::new(Mutex::new(Vec::new())),
                state_written: AtomicBool::new(false),
                lock_held: AtomicBool::new(true),
            }
        }

        async fn operation(&self, name: &'static str, latency: Duration) {
            self.operations.lock().unwrap().push(name);
            tokio::time::sleep(latency).await;
        }
    }

    #[async_trait::async_trait]
    impl StateBackend for LatencyBackend {
        async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
            Ok(None)
        }

        async fn write_state(&self, _state: &StateFile) -> BackendResult<()> {
            panic!("normal state write must not serve cancellation cleanup")
        }

        async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
            Ok(LockInfo::new(operation))
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
        ) -> BackendResult<()> {
            self.operation("cleanup lock GET", self.state_operation_latency)
                .await;
            self.operation("cleanup lock renewal PUT", self.state_operation_latency)
                .await;
            self.operation("cleanup state PUT", self.state_operation_latency)
                .await;
            self.state_written.store(true, Ordering::SeqCst);
            assert!(self.lock_held.load(Ordering::SeqCst));
            let _ = lock;
            Ok(())
        }

        async fn release_lock_for_cleanup(&self, _lock: &LockInfo) -> BackendResult<()> {
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

        async fn init(&self) -> BackendResult<()> {
            Ok(())
        }

        async fn bucket_exists(&self) -> BackendResult<bool> {
            Ok(true)
        }

        async fn create_bucket(&self) -> BackendResult<()> {
            Ok(())
        }
    }
}
