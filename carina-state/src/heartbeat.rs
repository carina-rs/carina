use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{BackendError, LockInfo, StateBackend};

/// Starts the single renewal task that owns a state lock during execution.
pub struct LockHeartbeat;

/// Owns both the lock capability and its renewal task.
///
/// Call [`HeartbeatGuard::stop`] before saving state or releasing the lock.
/// Dropping the guard aborts the task, which is appropriate for unwind paths
/// where the lock capability cannot be recovered.
pub struct HeartbeatGuard {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<LockInfo>>,
}

impl LockHeartbeat {
    /// Start renewing `lock` every third of its TTL.
    pub fn start(
        backend: Arc<dyn StateBackend>,
        mut lock: LockInfo,
        on_lost: impl FnOnce(String) + Send + 'static,
    ) -> HeartbeatGuard {
        let interval = (lock.renewal_ttl() / 3).max(Duration::from_millis(1));
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut on_lost = Some(on_lost);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => return lock,
                    _ = tokio::time::sleep(interval) => {}
                }

                match backend.renew_lock(&lock).await {
                    Ok(renewed) => lock = renewed,
                    Err(error @ BackendError::LockNotHeld(_)) => {
                        on_lost
                            .take()
                            .expect("lock-loss callback must only run once")(
                            error.to_string()
                        );
                        let _ = stop_rx.await;
                        return lock;
                    }
                    Err(_) => {
                        // A transient backend failure leaves the current lock
                        // capability intact. Retry on the next tick.
                    }
                }
            }
        });

        HeartbeatGuard {
            stop: Some(stop_tx),
            task: Some(task),
        }
    }
}

impl HeartbeatGuard {
    /// Stop renewals, join any renewal already in progress, and recover the
    /// most recently renewed lock capability.
    pub async fn stop(mut self) -> LockInfo {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let lock = self
            .task
            .as_mut()
            .expect("heartbeat task must exist until stop")
            .await
            .expect("state lock heartbeat task panicked");
        self.task.take();
        lock
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::{Notify, oneshot};

    use super::LockHeartbeat;
    use crate::{BackendError, BackendResult, LoadedState, LockInfo, StateBackend, StateFile};

    struct RecordingBackend {
        renewals: AtomicUsize,
        renewing: AtomicBool,
        save_overlapped_renewal: AtomicBool,
        renewal_started: Notify,
        renewal_delay: Duration,
        lose_lock: AtomicBool,
        transient_failures: AtomicUsize,
    }

    impl RecordingBackend {
        fn new(renewal_delay: Duration) -> Self {
            Self {
                renewals: AtomicUsize::new(0),
                renewing: AtomicBool::new(false),
                save_overlapped_renewal: AtomicBool::new(false),
                renewal_started: Notify::new(),
                renewal_delay,
                lose_lock: AtomicBool::new(false),
                transient_failures: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl StateBackend for RecordingBackend {
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

        async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo> {
            if self.lose_lock.load(Ordering::SeqCst) {
                return Err(BackendError::LockNotHeld(
                    "test lock was replaced".to_string(),
                ));
            }
            if self.transient_failures.load(Ordering::SeqCst) > 0 {
                self.transient_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(BackendError::Io("temporary test failure".to_string()));
            }
            self.renewing.store(true, Ordering::SeqCst);
            self.renewal_started.notify_waiters();
            tokio::time::sleep(self.renewal_delay).await;
            self.renewals.fetch_add(1, Ordering::SeqCst);
            self.renewing.store(false, Ordering::SeqCst);
            Ok(lock.renewed_for_test())
        }

        async fn write_state_locked(
            &self,
            _state: &StateFile,
            _lock: &LockInfo,
        ) -> BackendResult<()> {
            if self.renewing.load(Ordering::SeqCst) {
                self.save_overlapped_renewal.store(true, Ordering::SeqCst);
            }
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
    async fn heartbeat_renews_and_stop_returns_the_latest_lock() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(60));
        let original_expiry = lock.expires();
        let guard =
            LockHeartbeat::start(backend.clone(), lock, |_| panic!("lock should not be lost"));

        tokio::time::sleep(Duration::from_millis(75)).await;
        let renewed = guard.stop().await;

        assert!(backend.renewals.load(Ordering::SeqCst) >= 2);
        assert!(renewed.expires() > original_expiry);
    }

    #[tokio::test]
    async fn stop_joins_an_in_progress_renewal_before_save() {
        let backend = Arc::new(RecordingBackend::new(Duration::from_millis(50)));
        let lock = LockInfo::for_test_with_timeout("destroy", Duration::from_millis(30));
        let guard =
            LockHeartbeat::start(backend.clone(), lock, |_| panic!("lock should not be lost"));

        backend.renewal_started.notified().await;
        let lock = guard.stop().await;
        backend
            .write_state_locked(&StateFile::new(), &lock)
            .await
            .unwrap();

        assert_eq!(backend.renewals.load(Ordering::SeqCst), 1);
        assert!(!backend.save_overlapped_renewal.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn transient_renewal_failure_is_retried_on_the_next_tick() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        backend.transient_failures.store(1, Ordering::SeqCst);
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let guard =
            LockHeartbeat::start(backend.clone(), lock, |_| panic!("lock should not be lost"));

        tokio::time::sleep(Duration::from_millis(45)).await;
        let _lock = guard.stop().await;

        assert_eq!(backend.transient_failures.load(Ordering::SeqCst), 0);
        assert!(backend.renewals.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn lock_not_held_is_reported_by_the_guard() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        backend.lose_lock.store(true, Ordering::SeqCst);
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let (lost_tx, lost_rx) = oneshot::channel();
        let guard = LockHeartbeat::start(backend, lock, move |error| {
            let _ = lost_tx.send(error);
        });

        let error = lost_rx.await.expect("LockNotHeld should be observable");
        assert!(error.contains("test lock was replaced"));
        let _lock = guard.stop().await;
    }
}
