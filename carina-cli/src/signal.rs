//! Unified shutdown signal handling.
//!
//! Listens for SIGINT (Ctrl+C) and SIGTERM (e.g. GitHub Actions step cancel)
//! and fires a CancellationToken on the first signal. A second signal of
//! either kind force-exits the process with code 130 after restoring the
//! cursor.

use std::future::Future;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::error::AppError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

/// Trait for the source of shutdown signals. Production uses
/// `SignalEvents::unix()`; tests use `SignalEvents::from_receiver(rx)`.
pub trait ShutdownEvents: Send + 'static {
    fn recv(&mut self) -> impl Future<Output = Option<ShutdownSignal>> + Send;
}

/// Trait for the process exit mechanism. Production uses ProcessExit;
/// tests use RecordingExit to verify the call without actually exiting.
pub trait ExitProcess: Send + Sync + 'static {
    fn exit(&self, code: i32);
}

pub struct ProcessExit;

impl ExitProcess for ProcessExit {
    fn exit(&self, code: i32) {
        crate::cursor::restore_cursor();
        std::process::exit(code);
    }
}

pub enum SignalEvents {
    Unix {
        interrupt: Signal,
        terminate: Signal,
    },
    #[cfg(test)]
    Receiver(tokio::sync::mpsc::UnboundedReceiver<ShutdownSignal>),
}

impl SignalEvents {
    pub fn unix() -> std::io::Result<Self> {
        Ok(Self::Unix {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    #[cfg(test)]
    pub fn from_receiver(rx: tokio::sync::mpsc::UnboundedReceiver<ShutdownSignal>) -> Self {
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

pub fn spawn_shutdown_listener(token: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let events = SignalEvents::unix().expect("install unix signal handlers");
        listen_for_shutdown_events(token, events, ProcessExit).await;
    })
}

async fn listen_for_shutdown_events<E, X>(token: CancellationToken, mut events: E, exit: X)
where
    E: ShutdownEvents,
    X: ExitProcess,
{
    if events.recv().await.is_none() {
        return;
    }

    eprintln!("\nInterrupted! Cleaning up before exit...");
    token.cancel();

    if events.recv().await.is_none() {
        return;
    }

    eprintln!("\nForce exit.");
    exit.exit(130);
}

/// Read a single line from `reader`, cancellable by `cancel`.
///
/// Returns the line with any trailing `\n` or `\r\n` stripped, or
/// `Err(AppError::Interrupted)` if `cancel` fires first.
pub async fn read_line_until_cancelled<R>(
    reader: R,
    cancel: CancellationToken,
) -> Result<String, AppError>
where
    R: AsyncBufRead + Unpin,
{
    tokio::pin!(reader);
    let mut buf = String::new();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AppError::Interrupted),
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
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn read_line_until_cancelled_returns_input_when_token_not_cancelled() {
        let input = &b"yes\n"[..];
        let token = CancellationToken::new();
        let line = read_line_until_cancelled(input, token).await.unwrap();
        assert_eq!(line, "yes");
    }

    #[tokio::test]
    async fn read_line_until_cancelled_strips_crlf() {
        let input = &b"no\r\n"[..];
        let token = CancellationToken::new();
        let line = read_line_until_cancelled(input, token).await.unwrap();
        assert_eq!(line, "no");
    }

    #[tokio::test]
    async fn read_line_until_cancelled_returns_interrupted_when_token_is_cancelled() {
        // Simulates a user who hasn't pressed Enter at the confirmation prompt.
        let token = CancellationToken::new();
        token.cancel();
        let reader = tokio::io::BufReader::new(NeverReady);
        let err = read_line_until_cancelled(reader, token).await.unwrap_err();
        assert!(matches!(err, AppError::Interrupted));
    }

    #[tokio::test]
    async fn read_line_until_cancelled_returns_interrupted_when_cancel_fires_after_subscription() {
        let token = CancellationToken::new();
        let reader = tokio::io::BufReader::new(NeverReady);
        let waiting = tokio::spawn(read_line_until_cancelled(reader, token.clone()));
        tokio::task::yield_now().await;
        token.cancel();
        let err = waiting.await.unwrap().unwrap_err();
        assert!(matches!(err, AppError::Interrupted));
    }

    #[tokio::test]
    async fn signal_listener_cancels_token_on_interrupt_event() {
        let token = CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
        let exit = RecordingExit {
            calls: Arc::clone(&exit_calls),
        };

        let task = tokio::spawn(listen_for_shutdown_events(
            token.clone(),
            SignalEvents::from_receiver(rx),
            exit,
        ));
        tx.send(ShutdownSignal::Interrupt).unwrap();
        token.cancelled().await;
        drop(tx);
        task.await.unwrap();

        assert_eq!(*exit_calls.lock().unwrap(), Vec::<i32>::new());
    }

    #[tokio::test]
    async fn signal_listener_cancels_token_on_terminate_event() {
        let token = CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
        let exit = RecordingExit {
            calls: Arc::clone(&exit_calls),
        };

        let task = tokio::spawn(listen_for_shutdown_events(
            token.clone(),
            SignalEvents::from_receiver(rx),
            exit,
        ));
        tx.send(ShutdownSignal::Terminate).unwrap();
        token.cancelled().await;
        drop(tx);
        task.await.unwrap();

        assert_eq!(*exit_calls.lock().unwrap(), Vec::<i32>::new());
    }

    #[tokio::test]
    async fn terminate_and_interrupt_events_share_the_same_cancel_path() {
        // T12 contract: the listener treats Interrupt and Terminate identically.
        // Both fire token.cancel() and neither call exit on the first signal.
        // This pins the design so a future change cannot accidentally introduce
        // signal-kind-specific behavior at the listener layer.
        for signal in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate] {
            let token = CancellationToken::new();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
            let exit = RecordingExit {
                calls: Arc::clone(&exit_calls),
            };

            let task = tokio::spawn(listen_for_shutdown_events(
                token.clone(),
                SignalEvents::from_receiver(rx),
                exit,
            ));
            tx.send(signal).unwrap();
            token.cancelled().await;

            assert!(token.is_cancelled(), "{signal:?} must cancel the token");
            assert_eq!(
                *exit_calls.lock().unwrap(),
                Vec::<i32>::new(),
                "{signal:?} must not exit on first signal"
            );

            drop(tx);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn signal_listener_calls_exit_130_on_second_interrupt() {
        let token = CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let exit_calls = Arc::new(Mutex::new(Vec::<i32>::new()));
        let exit = RecordingExit {
            calls: Arc::clone(&exit_calls),
        };

        let task = tokio::spawn(listen_for_shutdown_events(
            token.clone(),
            SignalEvents::from_receiver(rx),
            exit,
        ));
        tx.send(ShutdownSignal::Interrupt).unwrap();
        token.cancelled().await;
        tx.send(ShutdownSignal::Interrupt).unwrap();
        task.await.unwrap();

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
