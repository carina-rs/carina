//! Graceful shutdown handling for SIGINT (Ctrl+C).
//!
//! Wraps a locked operation with `tokio::select!` so that Ctrl+C cancels the
//! operation and returns `AppError::Interrupted`, allowing the caller to release
//! the state lock before exiting.  A second Ctrl+C force-exits the process.

use std::future::Future;

use colored::Colorize;

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
