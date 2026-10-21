use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, oneshot};
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
    task: Option<JoinHandle<Result<(), BackendError>>>,
    lock: Arc<Mutex<Option<LockInfo>>>,
}

/// A heartbeat renewal proved that this process no longer owns the state lock.
#[derive(Debug)]
pub struct LockLost {
    error: BackendError,
}

impl LockLost {
    /// Consume the terminal diagnostic and return its backend detail.
    pub fn into_error(self) -> BackendError {
        self.error
    }
}

impl std::fmt::Display for LockLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "state lock heartbeat lost ownership: {}", self.error)
    }
}

impl std::error::Error for LockLost {}

impl LockHeartbeat {
    /// Start renewing `lock` every third of its TTL.
    pub fn start(
        backend: Arc<dyn StateBackend>,
        lock: LockInfo,
        on_lost: impl FnOnce() + Send + 'static,
    ) -> HeartbeatGuard {
        let interval = (lock.renewal_ttl() / 3).max(Duration::from_millis(1));
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let lock = Arc::new(Mutex::new(Some(lock)));
        let task_lock = Arc::clone(&lock);
        let task = tokio::spawn(async move {
            let mut on_lost = Some(on_lost);
            let mut warned_for_failure_streak = false;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => return Ok(()),
                    _ = tokio::time::sleep(interval) => {}
                }

                let mut lock = task_lock.lock().await;
                let current = lock
                    .as_ref()
                    .expect("heartbeat lock must exist until the guard stops");
                match backend.renew_lock(current).await {
                    Ok(renewed) => {
                        *lock = Some(renewed);
                        warned_for_failure_streak = false;
                    }
                    Err(error @ BackendError::LockNotHeld(_)) => {
                        let on_lost = on_lost
                            .take()
                            .expect("lock-loss callback must only run once");
                        on_lost();
                        return Err(error);
                    }
                    Err(error) => {
                        // A transient backend failure leaves the current lock
                        // capability intact. Retry on the next tick.
                        if !warned_for_failure_streak {
                            log::warn!(
                                "State lock heartbeat renewal failed; retrying on the next tick: {error}"
                            );
                            warned_for_failure_streak = true;
                        }
                    }
                }
            }
        });

        HeartbeatGuard {
            stop: Some(stop_tx),
            task: Some(task),
            lock,
        }
    }
}

impl HeartbeatGuard {
    /// Stop renewals, join any renewal already in progress, and recover the
    /// most recently renewed lock capability.
    pub async fn stop(mut self) -> Result<LockInfo, LockLost> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let outcome = self
            .task
            .as_mut()
            .expect("heartbeat task must exist until stop")
            .await
            .expect("state lock heartbeat task panicked");
        self.task.take();
        self.finish(outcome).await
    }

    /// Stop renewals without waiting for a renewal already in flight.
    ///
    /// Cleanup priority may have only a small process-exit budget. Aborting the
    /// task is safe because the last completed lock capability lives in the
    /// guard-owned slot, not in the renewal future.
    pub async fn stop_for_cleanup(mut self) -> Result<LockInfo, LockLost> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let outcome = if let Some(task) = self.task.take() {
            task.abort();
            match task.await {
                Ok(outcome) => outcome,
                Err(error) if error.is_cancelled() => Ok(()),
                Err(error) => panic!("state lock heartbeat task panicked: {error}"),
            }
        } else {
            Ok(())
        };
        self.finish(outcome).await
    }

    async fn take_lock(&self) -> LockInfo {
        self.lock
            .lock()
            .await
            .take()
            .expect("heartbeat guard must retain its lock capability")
    }

    async fn finish(&self, outcome: Result<(), BackendError>) -> Result<LockInfo, LockLost> {
        let lock = self.take_lock().await;
        match outcome {
            Ok(()) => Ok(lock),
            Err(error) => {
                drop(lock);
                Err(LockLost { error })
            }
        }
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
    use std::sync::{Mutex, Once};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::{Notify, oneshot};

    use super::LockHeartbeat;
    use crate::{BackendError, BackendResult, LoadedState, LockInfo, StateBackend, StateFile};

    struct WarningLogger {
        messages: Mutex<Vec<String>>,
    }

    impl log::Log for WarningLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() == log::Level::Warn && metadata.target() == "carina_state::heartbeat"
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                self.messages
                    .lock()
                    .expect("warning logger lock")
                    .push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    static WARNING_LOGGER: WarningLogger = WarningLogger {
        messages: Mutex::new(Vec::new()),
    };
    static INIT_WARNING_LOGGER: Once = Once::new();

    fn clear_captured_warnings() {
        INIT_WARNING_LOGGER.call_once(|| {
            log::set_logger(&WARNING_LOGGER).expect("install warning logger");
            log::set_max_level(log::LevelFilter::Warn);
        });
        WARNING_LOGGER
            .messages
            .lock()
            .expect("warning logger lock")
            .clear();
    }

    fn captured_warnings() -> Vec<String> {
        WARNING_LOGGER
            .messages
            .lock()
            .expect("warning logger lock")
            .clone()
    }

    struct RecordingBackend {
        renewals: AtomicUsize,
        renewing: AtomicBool,
        save_overlapped_renewal: AtomicBool,
        renewal_started: Notify,
        renewal_delay: Duration,
        renewal_never_completes: bool,
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
                renewal_never_completes: false,
                lose_lock: AtomicBool::new(false),
                transient_failures: AtomicUsize::new(0),
            }
        }

        fn with_never_completing_renewal() -> Self {
            Self {
                renewal_never_completes: true,
                ..Self::new(Duration::ZERO)
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
            if self.renewal_never_completes {
                std::future::pending::<()>().await;
            }
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
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));

        tokio::time::sleep(Duration::from_millis(75)).await;
        let renewed = guard.stop().await.expect("lock should remain held");

        assert!(backend.renewals.load(Ordering::SeqCst) >= 2);
        assert!(renewed.expires() > original_expiry);
    }

    #[tokio::test]
    async fn stop_joins_an_in_progress_renewal_before_save() {
        let backend = Arc::new(RecordingBackend::new(Duration::from_millis(50)));
        let lock = LockInfo::for_test_with_timeout("destroy", Duration::from_millis(30));
        let guard =
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));

        backend.renewal_started.notified().await;
        let lock = guard.stop().await.expect("lock should remain held");
        backend
            .write_state_locked(&StateFile::new(), &lock)
            .await
            .unwrap();

        assert_eq!(backend.renewals.load(Ordering::SeqCst), 1);
        assert!(!backend.save_overlapped_renewal.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cleanup_stop_aborts_an_in_progress_renewal_and_returns_the_last_lock() {
        let backend = Arc::new(RecordingBackend::with_never_completing_renewal());
        let renewal_started = backend.renewal_started.notified();
        tokio::pin!(renewal_started);
        let lock = LockInfo::for_test_with_timeout("destroy", Duration::from_millis(30));
        let lock_id = lock.id().to_owned();
        let guard =
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));

        renewal_started.await;
        let returned = tokio::time::timeout(Duration::from_millis(100), guard.stop_for_cleanup())
            .await
            .expect("cleanup-priority stop must not wait for a hung renewal")
            .expect("aborting an in-flight renewal does not prove lock loss");

        assert_eq!(returned.id(), lock_id);
    }

    #[tokio::test]
    async fn transient_renewal_failure_warns_once_per_failure_streak() {
        clear_captured_warnings();
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        backend.transient_failures.store(2, Ordering::SeqCst);
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let guard =
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));

        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.renewals.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("heartbeat should recover from the first failure streak");
        backend.transient_failures.store(2, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.renewals.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("heartbeat should recover from the second failure streak");
        let _lock = guard.stop().await.expect("lock should remain held");

        assert_eq!(backend.transient_failures.load(Ordering::SeqCst), 0);
        let warnings = captured_warnings();
        assert_eq!(warnings.len(), 2, "warnings: {warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|warning| warning.contains("I/O error: temporary test failure")),
            "warnings must preserve the backend diagnostic: {warnings:?}"
        );
    }

    #[tokio::test]
    async fn lock_not_held_is_reported_by_the_guard() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        backend.lose_lock.store(true, Ordering::SeqCst);
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let (lost_tx, lost_rx) = oneshot::channel();
        let guard = LockHeartbeat::start(backend, lock, move || {
            let _ = lost_tx.send(());
        });

        lost_rx.await.expect("LockNotHeld should be observable");
        let lost = guard.stop().await.expect_err("guard must report lock loss");
        let rendered = lost.to_string();
        assert!(rendered.contains("test lock was replaced"));
        assert_eq!(rendered.matches("test lock was replaced").count(), 1);
        assert!(
            std::error::Error::source(&lost).is_none(),
            "LockLost is a terminal diagnostic"
        );
        assert!(matches!(lost.into_error(), BackendError::LockNotHeld(_)));
    }
}
