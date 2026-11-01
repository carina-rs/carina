use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use carina_core::shutdown::{CleanupInterrupted, LoopStep, ShutdownToken};
use carina_state::{BackendError, LockedStateWriter, StateBackend, StateFile};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::commands::shared::lock_heartbeat::LockingDisabled;
use crate::error::AppError;

/// State-write capability used only while effect execution is still running.
///
/// The locked arm can only be obtained from the heartbeat guard, which makes
/// the heartbeat and incremental saves share one serialized lock owner. The
/// unlocked arm carries the explicit `--lock=false` witness.
#[derive(Clone)]
pub(crate) enum MidRunStateWriter {
    Locked(LockedStateWriter),
    Unlocked {
        backend: Arc<dyn StateBackend>,
        _disabled: LockingDisabled,
    },
}

impl MidRunStateWriter {
    pub(crate) fn locked(writer: LockedStateWriter) -> Self {
        Self::Locked(writer)
    }

    pub(crate) fn unlocked(backend: Arc<dyn StateBackend>, disabled: LockingDisabled) -> Self {
        Self::Unlocked {
            backend,
            _disabled: disabled,
        }
    }

    #[cfg(test)]
    fn unlocked_for_test(backend: Arc<dyn StateBackend>) -> Self {
        Self::unlocked(backend, LockingDisabled::for_test())
    }

    async fn write_state(&self, state: &mut StateFile) -> Result<(), AppError> {
        match self {
            Self::Locked(writer) => writer.write_state(state).await.map_err(AppError::Backend),
            Self::Unlocked {
                backend,
                _disabled: _,
            } => {
                state.increment_serial();
                backend.write_state(state).await.map_err(AppError::Backend)
            }
        }
    }

    /// Persist a graph-classified schema migration while the heartbeat owns
    /// the same lock used by later incremental writes.
    pub(crate) async fn write_schema_migration(
        &self,
        state: &mut StateFile,
    ) -> Result<(), AppError> {
        self.write_state(state).await
    }
}

/// Proof that the background saver has stopped before final state is built.
///
/// Production code cannot construct this directly. Consuming it is the only
/// way to obtain the state base used by the final write, which makes the final
/// write statically follow saver shutdown.
pub(crate) struct FinalStateBase {
    state: StateFile,
}

impl FinalStateBase {
    pub(crate) fn into_state(self) -> StateFile {
        self.state
    }

    #[cfg(test)]
    pub(crate) fn for_test(state: Option<StateFile>) -> Self {
        Self {
            state: state.unwrap_or_default(),
        }
    }
}

/// Coalescing background state saver.
///
/// While one write is in progress, a watch channel retains only the newest
/// cumulative snapshot. Normal stop waits for the current write and discards
/// anything queued behind it; cleanup-priority stop aborts it promptly. The
/// final write rebuilds from [`FinalStateBase`] and therefore supersedes any
/// discarded snapshot.
pub(crate) struct IncrementalStateSaver<S> {
    publisher: watch::Sender<Option<Arc<S>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    base_state: StateFile,
    latest_attempted_serial: Arc<AtomicU64>,
}

impl<S: Send + Sync + 'static> IncrementalStateSaver<S> {
    pub(crate) fn start(
        base_state: StateFile,
        writer: MidRunStateWriter,
        build_state: impl Fn(StateFile, &S) -> Result<StateFile, AppError> + Send + Sync + 'static,
    ) -> Self {
        let (sender, mut receiver) = watch::channel::<Option<Arc<S>>>(None);
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let latest_attempted_serial = Arc::new(AtomicU64::new(base_state.serial));
        let task_serial = Arc::clone(&latest_attempted_serial);
        let task_base = base_state.clone();
        let task = tokio::spawn(async move {
            let mut warned_for_failure_streak = false;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }

                let Some(snapshot) = receiver.borrow_and_update().clone() else {
                    continue;
                };
                let mut state = match build_state(task_base.clone(), snapshot.as_ref()) {
                    Ok(state) => state,
                    Err(error) => {
                        if !warned_for_failure_streak {
                            log::warn!(
                                "Incremental state checkpoint failed; retrying with the next snapshot: {error}"
                            );
                            warned_for_failure_streak = true;
                        }
                        continue;
                    }
                };

                let previous_serial = task_serial.fetch_add(1, Ordering::SeqCst);
                state.serial = previous_serial;
                match writer.write_state(&mut state).await {
                    Ok(()) => warned_for_failure_streak = false,
                    Err(AppError::Backend(BackendError::LockNotHeld(_))) => break,
                    Err(error) => {
                        if !warned_for_failure_streak {
                            log::warn!(
                                "Incremental state checkpoint failed; retrying with the next snapshot: {error}"
                            );
                            warned_for_failure_streak = true;
                        }
                    }
                }

                match stop_rx.try_recv() {
                    Ok(()) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => break,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                }
            }
        });

        Self {
            publisher: sender,
            stop: Some(stop_tx),
            task: Some(task),
            base_state,
            latest_attempted_serial,
        }
    }

    pub(crate) fn publisher(&self) -> watch::Sender<Option<Arc<S>>> {
        self.publisher.clone()
    }

    pub(crate) async fn stop(mut self, shutdown: &ShutdownToken) -> FinalStateBase {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut task) = self.task.take() {
            match shutdown.cleanup_aware_loop().step() {
                LoopStep::Abandon => Self::abort_task(task).await,
                LoopStep::Continue(wait) => match wait.until_cleanup_priority(&mut task).await {
                    CleanupInterrupted::Completed(outcome) => {
                        outcome.expect("incremental state saver task panicked");
                    }
                    CleanupInterrupted::Abandoned => Self::abort_task(task).await,
                },
            }
        }
        self.final_state_base()
    }

    async fn abort_task(task: JoinHandle<()>) {
        task.abort();
        match task.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => panic!("incremental state saver task panicked: {error}"),
        }
    }

    fn final_state_base(&self) -> FinalStateBase {
        let mut state = self.base_state.clone();
        state.serial = self.latest_attempted_serial.load(Ordering::SeqCst);
        FinalStateBase { state }
    }
}

impl<S> Drop for IncrementalStateSaver<S> {
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

    use async_trait::async_trait;
    use carina_core::shutdown::ShutdownToken;
    use carina_state::{
        BackendError, BackendResult, LoadedState, LockInfo, StateBackend, StateFile,
    };
    use tokio::sync::{Notify, Semaphore};

    use super::{IncrementalStateSaver, MidRunStateWriter};
    use crate::commands::apply::save_state_unlocked;
    use crate::commands::shared::lock_heartbeat::StateLock;

    struct RecordingBackend {
        writes: std::sync::Mutex<Vec<u64>>,
        serials: std::sync::Mutex<Vec<u64>>,
        attempts: AtomicUsize,
        block_first: AtomicBool,
        first_started: Notify,
        release_first: Semaphore,
        write_completed: Notify,
        lose_lock: AtomicBool,
    }

    impl RecordingBackend {
        fn slow_first() -> Self {
            Self {
                writes: std::sync::Mutex::new(Vec::new()),
                serials: std::sync::Mutex::new(Vec::new()),
                attempts: AtomicUsize::new(0),
                block_first: AtomicBool::new(true),
                first_started: Notify::new(),
                release_first: Semaphore::new(0),
                write_completed: Notify::new(),
                lose_lock: AtomicBool::new(false),
            }
        }

        fn immediate() -> Self {
            Self {
                block_first: AtomicBool::new(false),
                ..Self::slow_first()
            }
        }

        fn marker(state: &StateFile) -> u64 {
            state
                .exports
                .get("checkpoint")
                .and_then(|value| serde_json::to_value(value).ok())
                .and_then(|value| value.as_u64())
                .expect("test state must contain a checkpoint marker")
        }

        fn writes(&self) -> Vec<u64> {
            self.writes.lock().expect("writes lock").clone()
        }

        fn serials(&self) -> Vec<u64> {
            self.serials.lock().expect("serials lock").clone()
        }
    }

    #[async_trait]
    impl StateBackend for RecordingBackend {
        async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
            Ok(None)
        }

        async fn read_state_bytes(&self) -> BackendResult<Option<carina_state::RawState>> {
            Ok(None)
        }

        async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.lose_lock.load(Ordering::SeqCst) {
                self.write_completed.notify_one();
                return Err(BackendError::LockNotHeld(
                    "test lock was replaced".to_string(),
                ));
            }
            if attempt == 0 && self.block_first.load(Ordering::SeqCst) {
                self.first_started.notify_one();
                self.release_first
                    .acquire()
                    .await
                    .expect("release semaphore")
                    .forget();
            }
            self.writes
                .lock()
                .expect("writes lock")
                .push(Self::marker(state));
            self.serials
                .lock()
                .expect("serials lock")
                .push(state.serial);
            self.write_completed.notify_one();
            Ok(())
        }

        async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
            Ok(LockInfo::for_test(operation))
        }

        async fn release_lock(&self, _lock: &LockInfo) -> BackendResult<()> {
            Ok(())
        }

        async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo> {
            Ok(lock.renewed_for_test())
        }

        async fn write_state_locked(
            &self,
            state: &StateFile,
            _lock: &LockInfo,
        ) -> BackendResult<()> {
            self.write_state(state).await
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

    fn start_saver(backend: Arc<RecordingBackend>) -> IncrementalStateSaver<u64> {
        IncrementalStateSaver::start(
            StateFile::new(),
            MidRunStateWriter::unlocked_for_test(backend),
            |mut state, checkpoint| {
                state.insert_export_json_for_test("checkpoint", serde_json::json!(checkpoint));
                Ok(state)
            },
        )
    }

    #[tokio::test]
    async fn slow_write_coalesces_queued_snapshots_to_the_newest() {
        let backend = Arc::new(RecordingBackend::slow_first());
        let saver = start_saver(backend.clone());
        let publisher = saver.publisher();

        publisher.send_replace(Some(Arc::new(1)));
        backend.first_started.notified().await;
        publisher.send_replace(Some(Arc::new(2)));
        publisher.send_replace(Some(Arc::new(3)));
        backend.release_first.add_permits(1);
        while backend.writes().len() < 2 {
            backend.write_completed.notified().await;
        }

        let _final_base = saver.stop(&ShutdownToken::running()).await;
        assert_eq!(backend.writes(), [1, 3]);
        assert_eq!(backend.serials(), [1, 2]);
    }

    #[tokio::test]
    async fn stopping_saver_before_final_write_makes_final_write_last() {
        let backend = Arc::new(RecordingBackend::immediate());
        let saver = start_saver(backend.clone());
        let publisher = saver.publisher();
        publisher.send_replace(Some(Arc::new(1)));
        backend.write_completed.notified().await;

        let final_base = saver.stop(&ShutdownToken::running()).await;
        let mut final_state = final_base.into_state();
        final_state.insert_export_json_for_test("checkpoint", serde_json::json!(99));
        let lock = StateLock::disabled_for_test();
        let super::super::lock_heartbeat::StateLockRef::Disabled(disabled) = lock.as_ref() else {
            unreachable!()
        };
        save_state_unlocked(backend.as_ref(), disabled, &mut final_state)
            .await
            .unwrap();

        assert_eq!(backend.writes(), [1, 99]);
        assert_eq!(backend.serials(), [1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_stop_aborts_an_in_flight_checkpoint_before_final_write() {
        let backend = Arc::new(RecordingBackend::slow_first());
        let saver = start_saver(backend.clone());
        saver.publisher().send_replace(Some(Arc::new(1)));
        backend.first_started.notified().await;
        let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();
        trigger.prioritize_cleanup();

        let final_base =
            tokio::time::timeout(std::time::Duration::from_millis(100), saver.stop(&shutdown))
                .await
                .expect("cleanup stop must not wait for the blocked checkpoint write");
        let mut final_state = final_base.into_state();
        final_state.insert_export_json_for_test("checkpoint", serde_json::json!(99));
        let lock = StateLock::disabled_for_test();
        let super::super::lock_heartbeat::StateLockRef::Disabled(disabled) = lock.as_ref() else {
            unreachable!()
        };
        save_state_unlocked(backend.as_ref(), disabled, &mut final_state)
            .await
            .unwrap();

        assert_eq!(backend.writes(), [99]);
        assert_eq!(backend.serials(), [2]);
    }

    #[tokio::test(start_paused = true)]
    async fn normal_stop_escalates_to_cleanup_while_a_checkpoint_is_hung() {
        let backend = Arc::new(RecordingBackend::slow_first());
        let saver = start_saver(backend.clone());
        saver.publisher().send_replace(Some(Arc::new(1)));
        backend.first_started.notified().await;
        let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();

        let mut stop_task = tokio::spawn(async move { saver.stop(&shutdown).await });
        tokio::task::yield_now().await;
        trigger.prioritize_cleanup();

        let outcome =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut stop_task).await;
        if outcome.is_err() {
            stop_task.abort();
            let _ = stop_task.await;
            panic!("cleanup escalation must abort the already-joining checkpoint write");
        }
        let final_base = outcome
            .expect("checked above")
            .expect("stop task must not panic");

        assert_eq!(final_base.into_state().serial, 1);
        assert!(backend.writes().is_empty());
    }

    #[tokio::test]
    async fn lock_not_held_stops_saver_without_consuming_later_snapshots() {
        let backend = Arc::new(RecordingBackend::immediate());
        backend.lose_lock.store(true, Ordering::SeqCst);
        let saver = start_saver(backend.clone());
        let publisher = saver.publisher();
        publisher.send_replace(Some(Arc::new(1)));
        backend.write_completed.notified().await;
        publisher.send_replace(Some(Arc::new(2)));

        let _final_base = saver.stop(&ShutdownToken::running()).await;
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        assert!(backend.writes().is_empty());
    }
}
