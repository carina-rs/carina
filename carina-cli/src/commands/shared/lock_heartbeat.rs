use std::sync::Arc;

use carina_core::shutdown::ShutdownToken;
use carina_state::{LockHeartbeat, LockInfo, StateBackend};

/// Run one command phase while the heartbeat owns its optional lock, then
/// stop the heartbeat and return the lock capability with the phase result.
pub(crate) async fn with_lock_heartbeat<T>(
    backend: Arc<dyn StateBackend>,
    lock: Option<LockInfo>,
    shutdown: ShutdownToken,
    operation: impl AsyncFnOnce(ShutdownToken) -> T,
) -> (T, Option<LockInfo>) {
    match lock {
        Some(lock) => {
            let (trigger, run_shutdown) = shutdown.with_graceful_shutdown_trigger();
            let heartbeat = LockHeartbeat::start(backend, lock, move |error| {
                eprintln!("State lock heartbeat lost ownership: {error}");
                trigger.request_graceful_shutdown();
            });
            let result = operation(run_shutdown).await;
            let lock = heartbeat.stop().await;
            (result, Some(lock))
        }
        None => {
            let result = operation(shutdown).await;
            (result, None)
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

    use super::with_lock_heartbeat;

    struct LostLockBackend {
        renewal_started: Notify,
        fail_renewal: Notify,
    }

    impl LostLockBackend {
        fn new() -> Self {
            Self {
                renewal_started: Notify::new(),
                fail_renewal: Notify::new(),
            }
        }
    }

    #[async_trait]
    impl StateBackend for LostLockBackend {
        async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
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

    #[tokio::test]
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
            Some(lock),
            shutdown.clone(),
            async move |run_shutdown| {
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

        let ((dispatch_count, phase), returned_lock) =
            tokio::time::timeout(Duration::from_secs(1), simulated_run)
                .await
                .expect("LockNotHeld should cancel the run")
                .expect("simulated run task should not panic");

        assert_eq!(phase, ShutdownPhase::Graceful);
        assert_eq!(dispatch_count, 1, "no new effect may be dispatched");
        assert!(returned_lock.is_some());
    }
}
