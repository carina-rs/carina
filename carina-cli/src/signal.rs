//! Graceful shutdown handling for SIGINT (Ctrl+C).
//!
//! Wraps a locked operation with `tokio::select!` so that Ctrl+C cancels the
//! operation and returns `AppError::Interrupted`, allowing the caller to release
//! the state lock before exiting.  A second Ctrl+C force-exits the process.

use std::future::Future;

use colored::Colorize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::error::AppError;

/// Run `op` until completion, or until the user presses Ctrl+C.
///
/// On first Ctrl+C the future is dropped (cancelled) and
/// `Err(AppError::Interrupted)` is returned so that the caller can clean up
/// (release locks, save partial state, etc.).
///
/// While waiting for the caller to finish cleanup, a *second* Ctrl+C
/// force-exits the process immediately (exit code 130, the Unix convention for
/// SIGINT).
pub async fn run_with_ctrl_c<F, T>(op: F) -> Result<T, AppError>
where
    F: Future<Output = Result<T, AppError>>,
{
    tokio::select! {
        result = op => result,
        _ = tokio::signal::ctrl_c() => {
            eprintln!(
                "\n{}",
                "Interrupted! Cleaning up before exit..."
                    .yellow()
                    .bold()
            );

            // Spawn a background task that listens for a second Ctrl+C.
            tokio::spawn(async {
                // The first ctrl_c() was already consumed; wait for another.
                let _ = tokio::signal::ctrl_c().await;
                eprintln!(
                    "\n{}",
                    "Force exit."
                        .red()
                        .bold()
                );
                std::process::exit(130);
            });

            Err(AppError::Interrupted)
        }
    }
}

/// Read a single line from `reader`, cancellable by `interrupt`.
///
/// Returns the line with any trailing `\n` or `\r\n` stripped, or
/// `Err(AppError::Interrupted)` if `interrupt` fires first.
pub async fn read_line_with_interrupt<R, F>(reader: R, interrupt: F) -> Result<String, AppError>
where
    R: AsyncBufRead + Unpin,
    F: Future<Output = ()>,
{
    tokio::pin!(reader);
    let mut buf = String::new();
    tokio::select! {
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
        _ = interrupt => Err(AppError::Interrupted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_line_returns_input_before_interrupt() {
        let input = &b"yes\n"[..];
        let interrupt = std::future::pending::<()>();
        let line = read_line_with_interrupt(input, interrupt).await.unwrap();
        assert_eq!(line, "yes");
    }

    #[tokio::test]
    async fn read_line_strips_crlf() {
        let input = &b"no\r\n"[..];
        let interrupt = std::future::pending::<()>();
        let line = read_line_with_interrupt(input, interrupt).await.unwrap();
        assert_eq!(line, "no");
    }

    #[tokio::test]
    async fn read_line_returns_interrupted_when_signal_fires_first() {
        // Simulates a user who hasn't pressed Enter at the confirmation prompt.
        let reader = tokio::io::BufReader::new(NeverReady);
        let interrupt = async {};
        let err = read_line_with_interrupt(reader, interrupt)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Interrupted));
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
