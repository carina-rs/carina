//! Unified, command-scoped shutdown supervision.
//!
//! The supervisor owns both the command future and the only signal-driven
//! process-exit capability. That API shape prevents a detached signal task
//! from exiting while the command future still owes state and lock cleanup.

use std::future::Future;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use carina_core::shutdown::CLEANUP_DEADLINE;
#[cfg(test)]
use carina_core::shutdown::ShutdownExitReason;
use carina_core::shutdown::{
    ExitProcess, ShutdownEvents, ShutdownSignal, ShutdownToken, Supervised,
    supervise_shutdown_events,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::error::AppError;

struct ProcessExit;

impl ExitProcess for ProcessExit {
    fn exit(&self, code: i32) {
        crate::cursor::restore_cursor();
        std::process::exit(code);
    }
}

enum SignalEvents {
    Unix {
        interrupt: Signal,
        terminate: Signal,
    },
    #[cfg(test)]
    Receiver(tokio::sync::mpsc::UnboundedReceiver<ShutdownSignal>),
}

impl SignalEvents {
    fn unix() -> std::io::Result<Self> {
        Ok(Self::Unix {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    #[cfg(test)]
    fn from_receiver(rx: tokio::sync::mpsc::UnboundedReceiver<ShutdownSignal>) -> Self {
        Self::Receiver(rx)
    }
}

impl ShutdownEvents for SignalEvents {
    #[allow(clippy::manual_async_fn)]
    fn recv(&mut self) -> impl Future<Output = Option<ShutdownSignal>> + Send {
        async move {
            match self {
                Self::Unix {
                    interrupt,
                    terminate,
                } => {
                    tokio::select! {
                        _ = interrupt.recv() => Some(ShutdownSignal::Interrupt),
                        _ = terminate.recv() => Some(ShutdownSignal::Terminate),
                    }
                }
                #[cfg(test)]
                Self::Receiver(rx) => rx.recv().await,
            }
        }
    }
}

/// Run a command under unified SIGINT/SIGTERM supervision.
pub async fn run_with_shutdown<F, Fut, T>(command: F) -> T
where
    F: FnOnce(ShutdownToken) -> Fut,
    Fut: Future<Output = T>,
{
    let events = SignalEvents::unix().expect("install unix signal handlers");
    match supervise_shutdown_events(command, events, ProcessExit).await {
        Supervised::Completed(output) => output,
        Supervised::ExitRequested(_) => {
            unreachable!("ProcessExit must not return after std::process::exit")
        }
    }
}

/// Read a single line from `reader`, cancellable by `cancel`.
///
/// Returns the line with any trailing `\n` or `\r\n` stripped, or
/// `Err(AppError::Interrupted)` if `cancel` fires first.
pub async fn read_line_until_cancelled<R>(
    reader: R,
    shutdown: ShutdownToken,
) -> Result<String, AppError>
where
    R: AsyncBufRead + Unpin,
{
    tokio::pin!(reader);
    let mut buf = String::new();
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err(AppError::Interrupted),
        result = reader.read_line(&mut buf) => {
            result.map_err(|e| AppError::Config(e.to_string()))?;
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') {
                    buf.pop();
                }
            }
            Ok(buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::shutdown::testing::{cleanup_priority_requested, shutdown_channel};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn read_line_until_cancelled_returns_input_when_token_not_cancelled() {
        let input = &b"yes\n"[..];
        let token = ShutdownToken::running();
        let line = read_line_until_cancelled(input, token).await.unwrap();
        assert_eq!(line, "yes");
    }

    #[tokio::test]
    async fn read_line_until_cancelled_strips_crlf() {
        let input = &b"no\r\n"[..];
        let token = ShutdownToken::running();
        let line = read_line_until_cancelled(input, token).await.unwrap();
        assert_eq!(line, "no");
    }

    #[tokio::test]
    async fn read_line_until_cancelled_returns_interrupted_when_token_is_cancelled() {
        // Simulates a user who hasn't pressed Enter at the confirmation prompt.
        let (trigger, token) = shutdown_channel();
        trigger.request_graceful_shutdown();
        let reader = tokio::io::BufReader::new(NeverReady);
        let err = read_line_until_cancelled(reader, token).await.unwrap_err();
        assert!(matches!(err, AppError::Interrupted));
    }

    #[tokio::test]
    async fn read_line_until_cancelled_returns_interrupted_when_cancel_fires_after_subscription() {
        let (trigger, token) = shutdown_channel();
        let reader = tokio::io::BufReader::new(NeverReady);
        let waiting = tokio::spawn(read_line_until_cancelled(reader, token));
        tokio::task::yield_now().await;
        trigger.request_graceful_shutdown();
        let err = waiting.await.unwrap().unwrap_err();
        assert!(matches!(err, AppError::Interrupted));
    }

    #[tokio::test]
    async fn signal_listener_cancels_token_on_interrupt_event() {
        assert_first_signal_requests_graceful_shutdown(ShutdownSignal::Interrupt).await;
    }

    #[tokio::test]
    async fn signal_listener_cancels_token_on_terminate_event() {
        assert_first_signal_requests_graceful_shutdown(ShutdownSignal::Terminate).await;
    }

    #[tokio::test]
    async fn terminate_and_interrupt_events_share_the_same_cancel_path() {
        // T12 contract: the supervisor treats Interrupt and Terminate identically.
        // Both request graceful shutdown and neither calls exit on the first signal.
        // This pins the design so a future change cannot accidentally introduce
        // signal-kind-specific behavior at the supervisor layer.
        for signal in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate] {
            assert_first_signal_requests_graceful_shutdown(signal).await;
        }
    }

    #[tokio::test]
    async fn signal_supervisor_calls_exit_130_after_second_signal_cleanup() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
        let exit = RecordingExit {
            calls: Arc::clone(&exit_calls),
        };

        let task = tokio::spawn(supervise_shutdown_events(
            |shutdown| async move {
                cleanup_priority_requested(&shutdown).await;
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));
        tx.send(ShutdownSignal::Interrupt).unwrap();
        tx.send(ShutdownSignal::Interrupt).unwrap();
        let outcome = task.await.unwrap();

        assert!(matches!(outcome, Supervised::ExitRequested(_)));
        assert_eq!(*exit_calls.lock().unwrap(), vec![130]);
    }

    #[tokio::test]
    async fn second_signal_during_cleanup_does_not_exit_before_state_flush_and_lock_release() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state_flushed = Arc::new(AtomicBool::new(false));
        let lock_released = Arc::new(AtomicBool::new(false));
        let exit_observations = Arc::new(Mutex::new(Vec::new()));
        let exit = RecordingCleanupExit {
            observations: Arc::clone(&exit_observations),
            state_flushed: Arc::clone(&state_flushed),
            lock_released: Arc::clone(&lock_released),
        };

        let cleanup_state_flushed = Arc::clone(&state_flushed);
        let cleanup_lock_released = Arc::clone(&lock_released);
        let supervisor = tokio::spawn(supervise_shutdown_events(
            move |shutdown| async move {
                shutdown.cancelled().await;
                cleanup_priority_requested(&shutdown).await;
                tokio::time::sleep(Duration::from_millis(25)).await;
                cleanup_state_flushed.store(true, Ordering::SeqCst);
                cleanup_lock_released.store(true, Ordering::SeqCst);
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));
        tx.send(ShutdownSignal::Interrupt).unwrap();
        tx.send(ShutdownSignal::Terminate).unwrap();

        let outcome = supervisor.await.unwrap();

        assert!(matches!(outcome, Supervised::ExitRequested(_)));
        assert_eq!(
            *exit_observations.lock().unwrap(),
            vec![(130, true, true)],
            "exit(130) must observe completed state flush and lock release"
        );
    }

    #[tokio::test]
    async fn completed_cleanup_wins_when_a_third_signal_is_already_pending() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state_flushed = Arc::new(AtomicBool::new(false));
        let lock_released = Arc::new(AtomicBool::new(false));
        let exit_observations = Arc::new(Mutex::new(Vec::new()));
        let exit = RecordingCleanupExit {
            observations: Arc::clone(&exit_observations),
            state_flushed: Arc::clone(&state_flushed),
            lock_released: Arc::clone(&lock_released),
        };

        let command_state_flushed = Arc::clone(&state_flushed);
        let command_lock_released = Arc::clone(&lock_released);
        let supervisor = tokio::spawn(supervise_shutdown_events(
            move |shutdown| async move {
                cleanup_priority_requested(&shutdown).await;
                command_state_flushed.store(true, Ordering::SeqCst);
                command_lock_released.store(true, Ordering::SeqCst);
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));

        // Queue the whole GitHub Actions-style sequence before the supervisor
        // runs. At the final select both cleanup and the third signal are ready.
        tx.send(ShutdownSignal::Interrupt).unwrap();
        tx.send(ShutdownSignal::Terminate).unwrap();
        tx.send(ShutdownSignal::Interrupt).unwrap();

        let outcome = supervisor.await.unwrap();

        assert!(matches!(outcome, Supervised::ExitRequested(_)));
        assert_eq!(
            *exit_observations.lock().unwrap(),
            vec![(130, true, true)],
            "an already-completable command must be harvested before a pending third signal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn event_stream_ending_before_first_signal_uses_bounded_cleanup() {
        assert_closed_event_stream_is_bounded(false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn event_stream_ending_after_first_signal_uses_bounded_cleanup() {
        assert_closed_event_stream_is_bounded(true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn slow_state_flush_reserves_time_for_lock_release() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state_flushed = Arc::new(AtomicBool::new(false));
        let lock_released = Arc::new(AtomicBool::new(false));
        let cleanup_started = Arc::new(Notify::new());
        let exit_observations = Arc::new(Mutex::new(Vec::new()));
        let exit = RecordingCleanupExit {
            observations: Arc::clone(&exit_observations),
            state_flushed: Arc::clone(&state_flushed),
            lock_released: Arc::clone(&lock_released),
        };

        let command_lock_released = Arc::clone(&lock_released);
        let command_cleanup_started = Arc::clone(&cleanup_started);
        let supervisor = tokio::spawn(supervise_shutdown_events(
            move |shutdown| async move {
                cleanup_priority_requested(&shutdown).await;
                let result = crate::commands::shared::finalize::finalize_after_execute(
                    |_| async move {
                        command_cleanup_started.notify_one();
                        std::future::pending::<Result<(), AppError>>().await
                    },
                    true,
                    &shutdown,
                )
                .await;
                assert!(matches!(result, Err(AppError::Interrupted)));
                command_lock_released.store(true, Ordering::SeqCst);
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));

        tx.send(ShutdownSignal::Interrupt).unwrap();
        tx.send(ShutdownSignal::Terminate).unwrap();
        cleanup_started.notified().await;
        tokio::time::advance(CLEANUP_DEADLINE).await;
        let outcome = supervisor.await.unwrap();

        assert!(matches!(outcome, Supervised::ExitRequested(_)));
        assert_eq!(
            *exit_observations.lock().unwrap(),
            vec![(130, false, true)],
            "a slow state flush must be abandoned early enough to start lock release"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn third_signal_exits_immediately_even_while_cleanup_is_pending() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cleanup_finished = Arc::new(AtomicBool::new(false));
        let exit_observations = Arc::new(Mutex::new(Vec::new()));
        let exit = RecordingCleanupExit {
            observations: Arc::clone(&exit_observations),
            state_flushed: Arc::clone(&cleanup_finished),
            lock_released: Arc::clone(&cleanup_finished),
        };

        let supervisor = tokio::spawn(supervise_shutdown_events(
            |shutdown| async move {
                cleanup_priority_requested(&shutdown).await;
                std::future::pending::<()>().await;
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));
        tx.send(ShutdownSignal::Interrupt).unwrap();
        tx.send(ShutdownSignal::Terminate).unwrap();
        tx.send(ShutdownSignal::Interrupt).unwrap();

        let outcome = supervisor.await.unwrap();

        assert!(matches!(
            outcome,
            Supervised::ExitRequested(ShutdownExitReason::ThirdSignal)
        ));
        assert_eq!(
            *exit_observations.lock().unwrap(),
            vec![(130, false, false)]
        );
    }

    async fn assert_first_signal_requests_graceful_shutdown(signal: ShutdownSignal) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let command_cancelled = Arc::clone(&cancelled);
        let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
        let exit = RecordingExit {
            calls: Arc::clone(&exit_calls),
        };
        let task = tokio::spawn(supervise_shutdown_events(
            move |shutdown| async move {
                shutdown.cancelled().await;
                command_cancelled.store(true, Ordering::SeqCst);
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));

        tx.send(signal).unwrap();
        let outcome = task.await.unwrap();

        assert!(matches!(outcome, Supervised::Completed(())));
        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(
            *exit_calls.lock().unwrap(),
            Vec::<i32>::new(),
            "{signal:?} must not exit on first signal"
        );
    }

    async fn assert_closed_event_stream_is_bounded(send_first_signal: bool) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if send_first_signal {
            tx.send(ShutdownSignal::Interrupt).unwrap();
        }
        drop(tx);

        let cleanup_priority_observed = Arc::new(AtomicBool::new(false));
        let command_observed = Arc::clone(&cleanup_priority_observed);
        let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
        let exit = RecordingExit {
            calls: Arc::clone(&exit_calls),
        };
        let supervisor = tokio::spawn(supervise_shutdown_events(
            move |shutdown| async move {
                cleanup_priority_requested(&shutdown).await;
                command_observed.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            },
            SignalEvents::from_receiver(rx),
            exit,
        ));

        let outcome = tokio::time::timeout(CLEANUP_DEADLINE + Duration::from_secs(1), supervisor)
            .await
            .expect("a closed signal stream must retain a cleanup deadline")
            .unwrap();

        assert!(matches!(outcome, Supervised::ExitRequested(_)));
        assert!(cleanup_priority_observed.load(Ordering::SeqCst));
        assert_eq!(*exit_calls.lock().unwrap(), vec![130]);
    }

    struct RecordingExit {
        calls: Arc<Mutex<Vec<i32>>>,
    }

    impl ExitProcess for RecordingExit {
        fn exit(&self, code: i32) {
            self.calls.lock().unwrap().push(code);
        }
    }

    struct RecordingCleanupExit {
        observations: Arc<Mutex<Vec<(i32, bool, bool)>>>,
        state_flushed: Arc<AtomicBool>,
        lock_released: Arc<AtomicBool>,
    }

    impl ExitProcess for RecordingCleanupExit {
        fn exit(&self, code: i32) {
            self.observations.lock().unwrap().push((
                code,
                self.state_flushed.load(Ordering::SeqCst),
                self.lock_released.load(Ordering::SeqCst),
            ));
        }
    }

    struct NeverReady;

    impl tokio::io::AsyncRead for NeverReady {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }
}
