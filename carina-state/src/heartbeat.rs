use std::sync::Arc;
use std::time::Duration;

use std::sync::Mutex as StdMutex;

use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;

use crate::{BackendError, LockInfo, StateBackend};

/// Starts the single renewal task that owns a state lock during execution.
pub struct LockHeartbeat;

/// Owns both the lock capability and its renewal task.
///
/// Use [`HeartbeatGuard::writer`] for state checkpoints while renewals are
/// active. Call [`HeartbeatGuard::stop`] before the final raw state save or
/// lock release. Dropping the guard aborts the task, which is appropriate for
/// unwind paths where the lock capability cannot be recovered.
pub struct HeartbeatGuard {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), BackendError>>>,
    backend: Arc<dyn StateBackend>,
    ownership: Arc<LockOwnership>,
}

/// Serialized state writer that shares the heartbeat's current lock capability.
///
/// Clones all point at the same lock slot. A write holds that slot's mutex and
/// uses the heartbeat's current capability, so it cannot race a renewal.
#[derive(Clone)]
pub struct LockedStateWriter {
    backend: Arc<dyn StateBackend>,
    ownership: Arc<LockOwnership>,
}

struct LockOwnership {
    lock: Mutex<Option<LockInfo>>,
    lost_message: StdMutex<Option<String>>,
    on_lost: StdMutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
    lost: Notify,
}

impl LockOwnership {
    fn new(lock: LockInfo, on_lost: impl FnOnce() + Send + 'static) -> Self {
        Self {
            lock: Mutex::new(Some(lock)),
            lost_message: StdMutex::new(None),
            on_lost: StdMutex::new(Some(Box::new(on_lost))),
            lost: Notify::new(),
        }
    }

    fn record_lock_loss(&self, error: &BackendError) {
        let BackendError::LockNotHeld(message) = error else {
            return;
        };
        let first_loss = {
            let mut recorded = self
                .lost_message
                .lock()
                .expect("lock-loss diagnostic mutex poisoned");
            if recorded.is_some() {
                false
            } else {
                *recorded = Some(message.clone());
                true
            }
        };
        if !first_loss {
            return;
        }
        if let Some(on_lost) = self
            .on_lost
            .lock()
            .expect("lock-loss callback mutex poisoned")
            .take()
        {
            on_lost();
        }
        self.lost.notify_one();
    }

    fn lock_loss_error(&self) -> Option<BackendError> {
        self.lost_message
            .lock()
            .expect("lock-loss diagnostic mutex poisoned")
            .as_ref()
            .map(|message| BackendError::LockNotHeld(message.clone()))
    }
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
        let ownership = Arc::new(LockOwnership::new(lock, on_lost));
        let task_ownership = Arc::clone(&ownership);
        let task_backend = Arc::clone(&backend);
        let task = tokio::spawn(async move {
            let mut warned_for_failure_streak = false;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => return Ok(()),
                    _ = task_ownership.lost.notified() => {
                        return Err(task_ownership.lock_loss_error().expect(
                            "lock-loss notification must carry a diagnostic",
                        ));
                    }
                    _ = tokio::time::sleep(interval) => {}
                }

                let mut lock = task_ownership.lock.lock().await;
                if let Some(error) = task_ownership.lock_loss_error() {
                    return Err(error);
                }
                let current = lock
                    .as_ref()
                    .expect("heartbeat lock must exist until the guard stops");
                match task_backend.renew_lock(current).await {
                    Ok(renewed) => {
                        *lock = Some(renewed);
                        warned_for_failure_streak = false;
                    }
                    Err(error @ BackendError::LockNotHeld(_)) => {
                        task_ownership.record_lock_loss(&error);
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
            backend,
            ownership,
        }
    }
}

impl LockedStateWriter {
    /// Write state with the heartbeat's current lock capability while
    /// excluding renewal from the same capability slot.
    pub async fn write_state(&self, state: &mut crate::StateFile) -> crate::BackendResult<()> {
        let slot = self.ownership.lock.lock().await;
        if let Some(error) = self.ownership.lock_loss_error() {
            return Err(error);
        }
        let current = slot
            .as_ref()
            .expect("heartbeat lock must exist while its writer is alive");
        state.increment_serial();
        let result = self.backend.write_state_locked(state, current).await;
        if let Err(error @ BackendError::LockNotHeld(_)) = &result {
            self.ownership.record_lock_loss(error);
        }
        result
    }
}

impl HeartbeatGuard {
    /// Obtain a state writer backed by the heartbeat's serialized lock slot.
    pub fn writer(&self) -> LockedStateWriter {
        LockedStateWriter {
            backend: Arc::clone(&self.backend),
            ownership: Arc::clone(&self.ownership),
        }
    }

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
        self.ownership
            .lock
            .lock()
            .await
            .take()
            .expect("heartbeat guard must retain its lock capability")
    }

    async fn finish(&self, outcome: Result<(), BackendError>) -> Result<LockInfo, LockLost> {
        let lock = self.take_lock().await;
        let outcome = match outcome {
            Ok(()) => self.ownership.lock_loss_error().map_or(Ok(()), Err),
            Err(error) => Err(error),
        };
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
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, Once};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::{Notify, Semaphore, oneshot};

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

    fn renewal_stamp(sequence: usize) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap()
            + chrono::Duration::milliseconds(i64::try_from(sequence).unwrap())
    }

    struct RecordingBackend {
        renewals: AtomicUsize,
        renewing: AtomicBool,
        save_overlapped_renewal: AtomicBool,
        write_used_latest_lock: AtomicBool,
        renewals_at_write: AtomicUsize,
        latest_renewed_at: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
        renewal_started: Notify,
        renewal_completed: Semaphore,
        renewal_delay: Duration,
        renewal_never_completes: bool,
        lose_lock: AtomicBool,
        transient_failure_script: Mutex<VecDeque<bool>>,
    }

    impl RecordingBackend {
        fn new(renewal_delay: Duration) -> Self {
            Self {
                renewals: AtomicUsize::new(0),
                renewing: AtomicBool::new(false),
                save_overlapped_renewal: AtomicBool::new(false),
                write_used_latest_lock: AtomicBool::new(false),
                renewals_at_write: AtomicUsize::new(usize::MAX),
                latest_renewed_at: Mutex::new(None),
                renewal_started: Notify::new(),
                renewal_completed: Semaphore::new(0),
                renewal_delay,
                renewal_never_completes: false,
                lose_lock: AtomicBool::new(false),
                transient_failure_script: Mutex::new(VecDeque::new()),
            }
        }

        fn with_never_completing_renewal() -> Self {
            Self {
                renewal_never_completes: true,
                ..Self::new(Duration::ZERO)
            }
        }

        fn with_transient_failure_script(script: impl IntoIterator<Item = bool>) -> Self {
            Self {
                transient_failure_script: Mutex::new(script.into_iter().collect()),
                ..Self::new(Duration::ZERO)
            }
        }
    }

    #[async_trait]
    impl StateBackend for RecordingBackend {
        async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
            Ok(None)
        }

        async fn read_state_bytes(&self) -> BackendResult<Option<crate::RawState>> {
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
            if self
                .transient_failure_script
                .lock()
                .expect("transient failure script lock")
                .pop_front()
                .unwrap_or(false)
            {
                return Err(BackendError::Io("temporary test failure".to_string()));
            }
            if self.renewing.swap(true, Ordering::SeqCst) {
                return Err(BackendError::LockNotHeld(
                    "concurrent renewal replaced the lock capability".to_string(),
                ));
            }
            self.renewal_started.notify_one();
            if self.renewal_never_completes {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(self.renewal_delay).await;
            let sequence = self.renewals.fetch_add(1, Ordering::SeqCst) + 1;
            let renewed = lock.renewed_at_for_test(renewal_stamp(sequence));
            *self.latest_renewed_at.lock().expect("latest renewed lock") = Some(renewed.created());
            self.renewal_completed.add_permits(1);
            self.renewing.store(false, Ordering::SeqCst);
            Ok(renewed)
        }

        async fn write_state_locked(
            &self,
            _state: &StateFile,
            lock: &LockInfo,
        ) -> BackendResult<()> {
            self.renewals_at_write
                .store(self.renewals.load(Ordering::SeqCst), Ordering::SeqCst);
            if self.renewing.load(Ordering::SeqCst) {
                self.save_overlapped_renewal.store(true, Ordering::SeqCst);
            }
            let used_latest = self
                .latest_renewed_at
                .lock()
                .expect("latest renewed lock")
                .is_some_and(|created| created == lock.created());
            self.write_used_latest_lock
                .store(used_latest, Ordering::SeqCst);
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
    async fn heartbeat_renews_and_stop_returns_the_latest_lock() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(60));
        let original_id = lock.id().to_owned();
        let guard =
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));

        tokio::time::timeout(
            Duration::from_secs(5),
            backend.renewal_completed.acquire_many(2),
        )
        .await
        .expect("heartbeat should complete two renewals")
        .expect("renewal completion semaphore must remain open")
        .forget();
        let renewed = guard.stop().await.expect("lock should remain held");

        assert!(backend.renewals.load(Ordering::SeqCst) >= 2);
        assert_eq!(renewed.id(), original_id);
        let last_sequence = backend.renewals.load(Ordering::SeqCst);
        assert_eq!(renewed.created(), renewal_stamp(last_sequence));
        assert_ne!(renewed.created(), renewal_stamp(1));
        assert_eq!(
            Some(renewed.created()),
            *backend.latest_renewed_at.lock().unwrap(),
            "stop must return the last completed renewal's capability"
        );
    }

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
    async fn writer_serializes_with_renewal_uses_latest_lock_and_heartbeat_continues() {
        let backend = Arc::new(RecordingBackend::new(Duration::from_millis(50)));
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let guard =
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));
        let writer = guard.writer();

        backend.renewal_started.notified().await;
        let mut state = StateFile::new();
        writer
            .write_state(&mut state)
            .await
            .expect("writer must wait for the renewal and keep ownership");
        let renewals_after_write = backend.renewals.load(Ordering::SeqCst);
        backend
            .renewal_completed
            .acquire()
            .await
            .expect("first heartbeat renewal completion must be recorded")
            .forget();

        tokio::time::timeout(Duration::from_secs(5), backend.renewal_completed.acquire())
            .await
            .expect("heartbeat must continue renewing after the write")
            .expect("renewal semaphore must remain open")
            .forget();
        let _lock = guard.stop().await.expect("lock should remain held");

        assert_eq!(state.serial, 1);
        assert_eq!(
            backend.renewals_at_write.load(Ordering::SeqCst),
            1,
            "checkpoint writes must reuse the heartbeat's current capability without renewing"
        );
        assert!(backend.renewals.load(Ordering::SeqCst) > renewals_after_write);
        assert!(
            !backend.save_overlapped_renewal.load(Ordering::SeqCst),
            "renewal and write must use the same serialized lock slot"
        );
        assert!(
            backend.write_used_latest_lock.load(Ordering::SeqCst),
            "state write must receive the capability returned by the heartbeat renewal"
        );
    }

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
    async fn transient_renewal_failure_warns_once_per_failure_streak() {
        clear_captured_warnings();
        let backend = Arc::new(RecordingBackend::with_transient_failure_script([
            true, true, false, true, true, false,
        ]));
        let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(30));
        let guard =
            LockHeartbeat::start(backend.clone(), lock, || panic!("lock should not be lost"));

        tokio::time::timeout(
            Duration::from_secs(5),
            backend.renewal_completed.acquire_many(2),
        )
        .await
        .expect("heartbeat should recover from both scripted failure streaks")
        .expect("renewal completion semaphore must remain open")
        .forget();
        let _lock = guard.stop().await.expect("lock should remain held");

        assert!(
            backend
                .transient_failure_script
                .lock()
                .expect("transient failure script lock")
                .is_empty(),
            "heartbeat must consume every scripted renewal outcome"
        );
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
