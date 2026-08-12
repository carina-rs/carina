//! Command-scoped, multi-phase shutdown supervision.
//!
//! Command code receives only [`ShutdownToken`]. It cannot mint the write
//! capability used by the signal supervisor:
//!
//! ```compile_fail
//! let _ = carina_core::shutdown::shutdown_channel();
//! ```

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

// GitHub Actions cancellation is a fixed
// SIGINT -> 7.5s -> SIGTERM -> 2.5s -> SIGKILL sequence. Keep Carina's hard
// cleanup deadline inside the unconfigurable SIGTERM-to-SIGKILL window.
pub const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);

// Reserve the second half of the two-second budget for lock release. State
// persistence and lock release are both remote operations for S3 backends;
// an even split prevents either operation from consuming the other's entire
// opportunity while retaining 500ms before GitHub Actions sends SIGKILL.
const STATE_FLUSH_BUDGET: Duration = Duration::from_secs(1);

/// The command-visible shutdown phase.
///
/// Synchronous state inspection returns this enum so a caller must account for
/// cleanup priority rather than independently sampling unrelated booleans.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPhase {
    Running,
    Graceful,
    CleanupPriority,
}

/// A shutdown event understood by the command supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

/// Source of command shutdown events.
pub trait ShutdownEvents: Send + 'static {
    fn recv(&mut self) -> impl Future<Output = Option<ShutdownSignal>> + Send;
}

/// Process-exit capability held by the supervisor, never by the command.
pub trait ExitProcess: Send + Sync + 'static {
    fn exit(&self, code: i32);
}

/// Result of supervising a command.
pub enum Supervised<T> {
    Completed(T),
    ExitRequested(ShutdownExitReason),
}

/// The event that made the shutdown supervisor request process exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownExitReason {
    CleanupCompleted,
    ThirdSignal,
    DeadlineExpired,
}

/// Read-only shutdown state passed through command and executor APIs.
///
/// A token always carries graceful cancellation, cleanup priority, and the
/// state-flush cutoff. A command cannot construct a token missing an escalation
/// phase and cannot advance any phase itself.
#[derive(Clone, Debug)]
pub struct ShutdownToken {
    graceful: CancellationToken,
    cleanup_priority: CancellationToken,
    state_flush_budget_expired: CancellationToken,
}

/// Private write capability owned only by [`supervise_shutdown_events`].
#[derive(Clone, Debug)]
struct ShutdownTrigger {
    graceful: CancellationToken,
    cleanup_priority: CancellationToken,
    state_flush_budget_expired: CancellationToken,
}

fn shutdown_channel() -> (ShutdownTrigger, ShutdownToken) {
    let graceful = CancellationToken::new();
    let cleanup_priority = CancellationToken::new();
    let state_flush_budget_expired = CancellationToken::new();
    (
        ShutdownTrigger {
            graceful: graceful.clone(),
            cleanup_priority: cleanup_priority.clone(),
            state_flush_budget_expired: state_flush_budget_expired.clone(),
        },
        ShutdownToken {
            graceful,
            cleanup_priority,
            state_flush_budget_expired,
        },
    )
}

impl ShutdownTrigger {
    fn request_graceful_shutdown(&self) {
        self.graceful.cancel();
    }

    fn prioritize_cleanup(&self) {
        // Cleanup priority is a strict escalation of graceful shutdown. Firing
        // both makes "priority without cancellation" unrepresentable.
        self.graceful.cancel();
        self.cleanup_priority.cancel();
    }

    fn expire_state_flush_budget(&self) {
        self.state_flush_budget_expired.cancel();
    }
}

impl ShutdownToken {
    /// Construct a token that can never be cancelled.
    ///
    /// This is useful to callers that need to invoke a cancellable API outside
    /// command supervision without exposing a write capability.
    pub fn running() -> Self {
        shutdown_channel().1
    }

    /// Inspect the current shutdown phase as one exhaustive state.
    pub fn phase(&self) -> ShutdownPhase {
        if self.cleanup_priority.is_cancelled() {
            ShutdownPhase::CleanupPriority
        } else if self.graceful.is_cancelled() {
            ShutdownPhase::Graceful
        } else {
            ShutdownPhase::Running
        }
    }

    /// Wait for graceful shutdown (or a later phase) to be requested.
    pub async fn cancelled(&self) {
        self.graceful.cancelled().await;
    }

    /// Wait for cleanup-priority mode to be requested.
    pub async fn cleanup_priority_requested(&self) {
        self.cleanup_priority.cancelled().await;
    }

    /// Wait until state flushing must yield the remaining budget to lock release.
    pub async fn state_flush_budget_expired(&self) {
        self.state_flush_budget_expired.cancelled().await;
    }
}

/// Run a command while retaining sole ownership of shutdown escalation and
/// process exit.
///
/// The command can only complete or return cleanup to this function. No
/// signal-driven exit path exists outside this ownership boundary.
pub async fn supervise_shutdown_events<E, X, F, Fut, T>(
    command: F,
    mut events: E,
    exit: X,
) -> Supervised<T>
where
    E: ShutdownEvents,
    X: ExitProcess,
    F: FnOnce(ShutdownToken) -> Fut,
    Fut: Future<Output = T>,
{
    let (trigger, token) = shutdown_channel();
    let command = command(token);
    tokio::pin!(command);

    let first = tokio::select! {
        output = &mut command => return Supervised::Completed(output),
        signal = events.recv() => signal,
    };
    let Some(first) = first else {
        return finish_after_event_stream_ended(&mut command, &mut events, &trigger, &exit).await;
    };
    log_received_signal(first);
    eprintln!("Interrupted! Cleaning up before exit...");
    trigger.request_graceful_shutdown();

    let second = tokio::select! {
        output = &mut command => return Supervised::Completed(output),
        signal = events.recv() => signal,
    };
    let Some(second) = second else {
        return finish_after_event_stream_ended(&mut command, &mut events, &trigger, &exit).await;
    };
    log_received_signal(second);
    eprintln!("Prioritizing state and lock cleanup; abandoning in-flight operations...");
    trigger.prioritize_cleanup();

    finish_cleanup_before_exit(&mut command, &mut events, true, &trigger, &exit).await
}

async fn finish_after_event_stream_ended<E, X, Fut, T>(
    command: &mut std::pin::Pin<&mut Fut>,
    events: &mut E,
    trigger: &ShutdownTrigger,
    exit: &X,
) -> Supervised<T>
where
    E: ShutdownEvents,
    X: ExitProcess,
    Fut: Future<Output = T>,
{
    eprintln!("Shutdown event stream ended; prioritizing cleanup with a bounded deadline.");
    trigger.prioritize_cleanup();
    finish_cleanup_before_exit(command, events, false, trigger, exit).await
}

async fn finish_cleanup_before_exit<E, X, Fut, T>(
    command: &mut std::pin::Pin<&mut Fut>,
    events: &mut E,
    mut events_open: bool,
    trigger: &ShutdownTrigger,
    exit: &X,
) -> Supervised<T>
where
    E: ShutdownEvents,
    X: ExitProcess,
    Fut: Future<Output = T>,
{
    let state_flush_budget = tokio::time::sleep(STATE_FLUSH_BUDGET);
    tokio::pin!(state_flush_budget);
    let cleanup_deadline = tokio::time::sleep(CLEANUP_DEADLINE);
    tokio::pin!(cleanup_deadline);
    let mut state_flush_budget_open = true;

    loop {
        tokio::select! {
            biased;
            // Harvest a completed command before considering a simultaneously
            // pending third signal. A genuinely pending command still yields
            // immediately to that signal in the next arm.
            output = &mut *command => {
                drop(output);
                eprintln!("Cancellation cleanup complete; exiting.");
                exit.exit(130);
                return Supervised::ExitRequested(ShutdownExitReason::CleanupCompleted);
            }
            signal = events.recv(), if events_open => {
                if let Some(third) = signal {
                    log_received_signal(third);
                    eprintln!("Force exit after third shutdown signal.");
                    exit.exit(130);
                    return Supervised::ExitRequested(ShutdownExitReason::ThirdSignal);
                }
                events_open = false;
            }
            _ = &mut state_flush_budget, if state_flush_budget_open => {
                state_flush_budget_open = false;
                eprintln!(
                    "State flush budget expired after {}; reserving time for state lock release.",
                    duration_for_log(STATE_FLUSH_BUDGET)
                );
                trigger.expire_state_flush_budget();
            }
            _ = &mut cleanup_deadline => {
                eprintln!(
                    "Cleanup deadline expired after {}; exiting.",
                    duration_for_log(CLEANUP_DEADLINE)
                );
                exit.exit(130);
                return Supervised::ExitRequested(ShutdownExitReason::DeadlineExpired);
            }
        }
    }
}

fn log_received_signal(signal: ShutdownSignal) {
    eprintln!("\nReceived shutdown signal: {signal:?}.");
}

fn duration_for_log(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        let seconds = duration.as_secs();
        let unit = if seconds == 1 { "second" } else { "seconds" };
        format!("{seconds} {unit}")
    } else {
        format!("{} milliseconds", duration.as_millis())
    }
}

/// Shutdown write access compiled only for tests of command/executor behavior.
#[cfg(any(test, feature = "test-shutdown"))]
pub mod testing {
    use super::{ShutdownToken, ShutdownTrigger};

    #[derive(Clone, Debug)]
    pub struct TestShutdownTrigger(ShutdownTrigger);

    pub fn shutdown_channel() -> (TestShutdownTrigger, ShutdownToken) {
        let (trigger, token) = super::shutdown_channel();
        (TestShutdownTrigger(trigger), token)
    }

    impl TestShutdownTrigger {
        pub fn request_graceful_shutdown(&self) {
            self.0.request_graceful_shutdown();
        }

        pub fn prioritize_cleanup(&self) {
            self.0.prioritize_cleanup();
        }

        pub fn expire_state_flush_budget(&self) {
            self.0.expire_state_flush_budget();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_deadline_fits_the_github_actions_sigterm_window() {
        let github_actions_term_to_kill = Duration::from_millis(2_500);

        assert!(
            CLEANUP_DEADLINE < github_actions_term_to_kill,
            "cleanup must finish before GitHub Actions sends SIGKILL 2.5s after SIGTERM"
        );
        assert_eq!(duration_for_log(CLEANUP_DEADLINE), "2 seconds");
    }

    #[tokio::test]
    async fn cleanup_priority_always_implies_graceful_shutdown() {
        let (trigger, token) = shutdown_channel();

        assert_eq!(token.phase(), ShutdownPhase::Running);
        trigger.request_graceful_shutdown();
        assert_eq!(token.phase(), ShutdownPhase::Graceful);

        trigger.prioritize_cleanup();

        assert_eq!(token.phase(), ShutdownPhase::CleanupPriority);
        token.cancelled().await;
        token.cleanup_priority_requested().await;
    }
}
