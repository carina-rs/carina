use std::sync::Arc;

use carina_core::shutdown::{ShutdownPhase, ShutdownToken};
use carina_state::{LockHeartbeat, LockInfo, LockLost, StateBackend};

use super::incremental_state_saver::MidRunStateWriter;

/// Proof that the CLI flag explicitly disabled state locking.
///
/// Its field and constructor stay private so an unlocked state write cannot be
/// authorized by an absent `LockInfo`.
#[derive(Clone, Debug)]
pub(crate) struct LockingDisabled {
    _private: (),
}

impl LockingDisabled {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self { _private: () }
    }
}

/// The command's state-lock mode after processing `--lock`.
#[derive(Debug)]
pub(crate) enum StateLock {
    Disabled(LockingDisabled),
    Held(LockInfo),
}

/// A borrow of the command's state-lock mode for save/finalize helpers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StateLockRef<'a> {
    Disabled(&'a LockingDisabled),
    Held(&'a LockInfo),
}

/// Result of a phase run under the optional state-lock heartbeat.
///
/// The lost variant intentionally carries no lock capability, so callers
/// cannot save state or release a lock that another process may now own.
pub(crate) enum HeartbeatOutcome<T> {
    Kept { result: T, lock: StateLock },
    Lost { result: T, lost: LockLost },
}

impl StateLock {
    pub(crate) fn held(lock: LockInfo) -> Self {
        Self::Held(lock)
    }

    /// Mint the unlocked-write witness at the `--lock=false` boundary.
    pub(crate) fn disabled_by_cli_flag() -> Self {
        Self::Disabled(LockingDisabled { _private: () })
    }

    #[cfg(test)]
    pub(crate) fn disabled_for_test() -> Self {
        Self::Disabled(LockingDisabled::for_test())
    }

    pub(crate) fn as_ref(&self) -> StateLockRef<'_> {
        match self {
            Self::Disabled(disabled) => StateLockRef::Disabled(disabled),
            Self::Held(lock) => StateLockRef::Held(lock),
        }
    }

    pub(crate) fn is_held(&self) -> bool {
        matches!(self, Self::Held(_))
    }
}

/// Run one command phase while the heartbeat owns its optional lock, then
/// stop the heartbeat and return the lock capability with the phase result.
pub(crate) async fn with_lock_heartbeat<T>(
    backend: Arc<dyn StateBackend>,
    lock: StateLock,
    shutdown: ShutdownToken,
    operation: impl AsyncFnOnce(ShutdownToken, MidRunStateWriter) -> T,
) -> HeartbeatOutcome<T> {
    match lock {
        StateLock::Held(lock) => {
            let (trigger, run_shutdown) = shutdown.with_graceful_shutdown_trigger();
            let heartbeat = LockHeartbeat::start(backend, lock, move || {
                trigger.request_graceful_shutdown();
            });
            let writer = MidRunStateWriter::locked(heartbeat.writer());
            let result = operation(run_shutdown, writer).await;
            let stopped = match shutdown.phase() {
                ShutdownPhase::CleanupPriority => heartbeat.stop_for_cleanup().await,
                ShutdownPhase::Running | ShutdownPhase::Graceful => heartbeat.stop().await,
            };
            match stopped {
                Ok(lock) => HeartbeatOutcome::Kept {
                    result,
                    lock: StateLock::Held(lock),
                },
                Err(lost) => HeartbeatOutcome::Lost { result, lost },
            }
        }
        StateLock::Disabled(disabled) => {
            let writer = MidRunStateWriter::unlocked(Arc::clone(&backend), disabled.clone());
            let result = operation(shutdown, writer).await;
            HeartbeatOutcome::Kept {
                result,
                lock: StateLock::Disabled(disabled),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use carina_core::shutdown::{ShutdownPhase, ShutdownToken};
    use carina_state::{
        BackendError, BackendResult, LoadedState, LockInfo, StateBackend, StateFile,
    };
    use tokio::sync::{Notify, oneshot};

    use super::{HeartbeatOutcome, StateLock, with_lock_heartbeat};

    struct LostLockBackend {
        renewal_started: Notify,
        fail_renewal: Notify,
        renewal_never_completes: bool,
    }

    impl LostLockBackend {
        fn new() -> Self {
            Self {
                renewal_started: Notify::new(),
                fail_renewal: Notify::new(),
                renewal_never_completes: false,
            }
        }

        fn with_never_completing_renewal() -> Self {
            Self {
                renewal_never_completes: true,
                ..Self::new()
            }
        }
    }

    #[async_trait]
    impl StateBackend for LostLockBackend {
        async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
            Ok(None)
        }

        async fn read_state_bytes(&self) -> BackendResult<Option<carina_state::RawState>> {
            Ok(None)
        }

        async fn write_state(&self, _state: &StateFile) -> BackendResult<()> {
            Ok(())
        }

        async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
            Ok(LockInfo::for_test(operation))
        }

        async fn release_lock(&self, _lock: &LockInfo) -> BackendResult<()> {
            Ok(())
        }

        async fn renew_lock(&self, _lock: &LockInfo) -> BackendResult<LockInfo> {
            self.renewal_started.notify_one();
            if self.renewal_never_completes {
                std::future::pending::<()>().await;
            }
            self.fail_renewal.notified().await;
            Err(BackendError::LockNotHeld(
                "test lock was replaced".to_string(),
            ))
        }

        async fn write_state_locked(
            &self,
            _state: &StateFile,
            _lock: &LockInfo,
        ) -> BackendResult<()> {
            Ok(())
        }

        async fn force_unlock(&self, _lock_id: &str) -> BackendResult<()> {
            Ok(())
        }

        async fn bucket_exists(&self) -> BackendResult<bool> {
            Ok(true)
        }

        async fn create_bucket(&self) -> BackendResult<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lock_not_held_cancels_before_another_dispatch() {
        let backend = Arc::new(LostLockBackend::new());
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let shutdown = ShutdownToken::running();

        let first_dispatched = Arc::new(Notify::new());
        let finish_first = Arc::new(Notify::new());
        let (run_shutdown_tx, run_shutdown_rx) = oneshot::channel();
        let run_first_dispatched = first_dispatched.clone();
        let run_finish_first = finish_first.clone();
        let simulated_run = tokio::spawn(with_lock_heartbeat(
            backend.clone(),
            StateLock::held(lock),
            shutdown.clone(),
            async move |run_shutdown, _writer| {
                let _ = run_shutdown_tx.send(run_shutdown.clone());
                let mut dispatch_count = 0;
                while matches!(run_shutdown.phase(), ShutdownPhase::Running) {
                    dispatch_count += 1;
                    run_first_dispatched.notify_one();
                    run_finish_first.notified().await;
                }
                (dispatch_count, run_shutdown.phase())
            },
        ));

        let observed_cancel = run_shutdown_rx
            .await
            .expect("execution should expose its derived shutdown token");
        first_dispatched.notified().await;
        backend.renewal_started.notified().await;
        backend.fail_renewal.notify_one();
        tokio::time::timeout(Duration::from_secs(1), observed_cancel.cancelled())
            .await
            .expect("LockNotHeld should cancel the run");
        finish_first.notify_one();

        let outcome = tokio::time::timeout(Duration::from_secs(1), simulated_run)
            .await
            .expect("LockNotHeld should cancel the run")
            .expect("simulated run task should not panic");
        let HeartbeatOutcome::Lost {
            result: (dispatch_count, phase),
            lost,
        } = outcome
        else {
            panic!("LockNotHeld must return the lost-lock variant");
        };

        assert_eq!(phase, ShutdownPhase::Graceful);
        assert_eq!(dispatch_count, 1, "no new effect may be dispatched");
        assert!(matches!(lost.into_error(), BackendError::LockNotHeld(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_priority_uses_bounded_stop_for_a_hung_renewal() {
        let backend = Arc::new(LostLockBackend::with_never_completing_renewal());
        let lock = LockInfo::for_test_with_timeout("destroy", Duration::from_millis(30));
        let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();
        let run_backend = backend.clone();

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            with_lock_heartbeat(
                backend,
                StateLock::held(lock),
                shutdown,
                async move |_, _writer| {
                    run_backend.renewal_started.notified().await;
                    trigger.prioritize_cleanup();
                },
            ),
        )
        .await
        .expect("cleanup priority must abort a hung heartbeat renewal");
        let HeartbeatOutcome::Kept {
            result: (),
            lock: returned_lock,
        } = outcome
        else {
            panic!("aborting an in-flight renewal must not report lock loss");
        };

        assert!(returned_lock.is_held());
    }
}
