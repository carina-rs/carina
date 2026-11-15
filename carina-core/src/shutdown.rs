//! Command-scoped, multi-phase shutdown supervision.
//!
//! Command code receives only [`ShutdownToken`]. It cannot mint the write
//! capability used by the signal supervisor:
//!
//! ```compile_fail
//! let _ = carina_core::shutdown::shutdown_channel();
//! ```

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

// The default fits GitHub Actions' 2.5s SIGTERM-to-SIGKILL window.
// Backends can publish longer limits; an external SIGKILL still cuts them off.
pub const CLEANUP_DEADLINE: Duration = CleanupBudget::TWO_SECOND.total();

/// Backend cleanup limits, counted from cleanup priority (the second signal).
///
/// The flush cutoff is measured from cleanup priority, leaving time for lock
/// release. Backend-internal I/O timeouts are separate from these limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupBudget {
    state_flush: Duration,
    total: Duration,
    reason: Option<CleanupWaitReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupWaitReason {
    GitHubCommits,
}

impl CleanupBudget {
    pub const TWO_SECOND: Self = Self::new(Duration::from_secs(1), Duration::from_secs(2));

    /// Require a nonzero flush window and reserve time for lock release.
    pub const fn new(state_flush: Duration, total: Duration) -> Self {
        assert!(!state_flush.is_zero() && state_flush.as_nanos() < total.as_nanos());
        Self {
            state_flush,
            total,
            reason: None,
        }
    }

    pub const fn with_reason(mut self, reason: CleanupWaitReason) -> Self {
        self.reason = Some(reason);
        self
    }

    pub const fn state_flush(self) -> Duration {
        self.state_flush
    }

    pub const fn total(self) -> Duration {
        self.total
    }
}

#[derive(Debug, Default)]
struct PublishedCleanupBudget {
    budget: OnceLock<CleanupBudget>,
    published: CancellationToken,
}

impl PublishedCleanupBudget {
    fn current(&self) -> CleanupBudget {
        self.budget
            .get()
            .copied()
            .unwrap_or(CleanupBudget::TWO_SECOND)
    }
}

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

/// Outcome of a wait that cleanup priority is allowed to abandon.
#[derive(Debug)]
pub enum CleanupInterrupted<T> {
    /// The awaited work finished before cleanup priority was requested.
    Completed(T),
    /// Cleanup priority fired; the work was abandoned mid-flight.
    Abandoned,
}

/// A non-cleanup shutdown phase captured when a cleanup-aware wait is issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopShutdownPhase {
    Running,
    Graceful,
}

/// Couples each cleanup-aware blocking wait to the phase observation that issued it.
#[derive(Debug)]
pub struct CleanupAwareLoop<'a> {
    shutdown: &'a ShutdownToken,
}

/// Whether the latest phase observation permits a cleanup-aware blocking wait.
#[derive(Debug)]
pub enum LoopStep<'a> {
    /// Continue with the only handle that can perform a cleanup-aware wait.
    Continue(CleanupWait<'a>),
    /// Cleanup priority is active, so the loop must be abandoned.
    Abandon,
}

/// A handle issued after observing the shutdown phase for a cleanup-aware wait.
#[derive(Debug)]
pub struct CleanupWait<'a> {
    shutdown: &'a ShutdownToken,
    phase: LoopShutdownPhase,
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
/// A token always carries graceful cancellation, cleanup priority, and its
/// start time. A command cannot construct a token missing an escalation
/// phase and cannot advance any phase itself.
#[derive(Clone, Debug)]
pub struct ShutdownToken {
    graceful: CancellationToken,
    cleanup_priority: CancellationToken,
    cleanup_started: Arc<OnceLock<tokio::time::Instant>>,
    cleanup_budget: Arc<PublishedCleanupBudget>,
    #[cfg(any(test, feature = "test-shutdown"))]
    test_channel_identity: Arc<()>,
}

/// Write capability for an operation-local graceful-shutdown child.
///
/// Requesting this trigger does not advance the command supervisor or its
/// parent token. It only asks work using the derived token to stop dispatching
/// new operations and drain work already in flight.
#[derive(Clone, Debug)]
pub struct GracefulShutdownTrigger {
    graceful: CancellationToken,
}

/// Private write capability owned only by [`supervise_shutdown_events`].
#[derive(Clone, Debug)]
struct ShutdownTrigger {
    graceful: CancellationToken,
    cleanup_priority: CancellationToken,
    cleanup_started: Arc<OnceLock<tokio::time::Instant>>,
    cleanup_budget: Arc<PublishedCleanupBudget>,
}

fn shutdown_channel() -> (ShutdownTrigger, ShutdownToken) {
    let graceful = CancellationToken::new();
    let cleanup_priority = CancellationToken::new();
    let cleanup_started = Arc::new(OnceLock::new());
    let cleanup_budget = Arc::new(PublishedCleanupBudget::default());
    (
        ShutdownTrigger {
            graceful: graceful.clone(),
            cleanup_priority: cleanup_priority.clone(),
            cleanup_started: Arc::clone(&cleanup_started),
            cleanup_budget: Arc::clone(&cleanup_budget),
        },
        ShutdownToken {
            graceful,
            cleanup_priority,
            cleanup_started,
            cleanup_budget,
            #[cfg(any(test, feature = "test-shutdown"))]
            test_channel_identity: Arc::new(()),
        },
    )
}

impl ShutdownTrigger {
    fn request_graceful_shutdown(&self) {
        self.graceful.cancel();
    }

    fn prioritize_cleanup(&self) {
        self.cleanup_started.get_or_init(tokio::time::Instant::now);
        // Cleanup priority is a strict escalation of graceful shutdown. Firing
        // both makes "priority without cancellation" unrepresentable.
        self.graceful.cancel();
        self.cleanup_priority.cancel();
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

    /// Derive a token that observes this command's shutdown phases and also
    /// has one operation-local graceful-shutdown trigger.
    pub fn with_graceful_shutdown_trigger(&self) -> (GracefulShutdownTrigger, Self) {
        let graceful = self.graceful.child_token();
        (
            GracefulShutdownTrigger {
                graceful: graceful.clone(),
            },
            Self {
                graceful,
                cleanup_priority: self.cleanup_priority.clone(),
                cleanup_started: Arc::clone(&self.cleanup_started),
                cleanup_budget: Arc::clone(&self.cleanup_budget),
                #[cfg(any(test, feature = "test-shutdown"))]
                test_channel_identity: Arc::clone(&self.test_channel_identity),
            },
        )
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

    /// Create a handle that observes shutdown before issuing cleanup-aware waits.
    pub fn cleanup_aware_loop(&self) -> CleanupAwareLoop<'_> {
        CleanupAwareLoop { shutdown: self }
    }

    /// Wait for graceful shutdown (or a later phase) to be requested.
    pub async fn cancelled(&self) {
        self.graceful.cancelled().await;
    }

    /// Publish this command's backend limits once, shared by all child tokens.
    /// Reopening the same kind of backend is harmless; changing limits is rejected.
    pub fn publish_cleanup_budget(&self, budget: CleanupBudget) -> Result<(), CleanupBudget> {
        match self.cleanup_budget.budget.set(budget) {
            Ok(()) => {
                self.cleanup_budget.published.cancel();
                Ok(())
            }
            Err(_) if self.cleanup_budget.current() == budget => Ok(()),
            Err(rejected) => Err(rejected),
        }
    }

    pub fn cleanup_budget(&self) -> CleanupBudget {
        self.cleanup_budget.current()
    }

    /// Absolute supervisor deadline, anchored to the start of cleanup priority.
    pub fn cleanup_deadline(&self) -> Option<tokio::time::Instant> {
        self.cleanup_started
            .get()
            .map(|started| *started + self.cleanup_budget().total())
    }

    /// Wait until this backend's flush budget must yield to lock release.
    /// A late finalizer receives only the time remaining since cleanup priority.
    pub async fn state_flush_budget_expired(&self, budget: CleanupBudget) {
        self.cleanup_priority.cancelled().await;
        let started = self.cleanup_started.get().expect("cleanup priority start");
        tokio::time::sleep_until(*started + budget.state_flush()).await;
    }
}

impl GracefulShutdownTrigger {
    /// Ask users of the derived token to stop new dispatch and drain in-flight work.
    pub fn request_graceful_shutdown(&self) {
        self.graceful.cancel();
    }
}

impl<'a> CleanupAwareLoop<'a> {
    /// Re-sample shutdown and issue one cleanup-aware wait handle when permitted.
    ///
    /// This may be called anywhere the loop handle is in scope, including before
    /// a long nested wait.
    pub fn step(&self) -> LoopStep<'a> {
        match self.shutdown.phase() {
            ShutdownPhase::Running => LoopStep::Continue(CleanupWait {
                shutdown: self.shutdown,
                phase: LoopShutdownPhase::Running,
            }),
            ShutdownPhase::Graceful => LoopStep::Continue(CleanupWait {
                shutdown: self.shutdown,
                phase: LoopShutdownPhase::Graceful,
            }),
            ShutdownPhase::CleanupPriority => LoopStep::Abandon,
        }
    }
}

impl CleanupWait<'_> {
    /// Return the non-cleanup phase observed when this wait handle was issued.
    pub fn phase(&self) -> LoopShutdownPhase {
        self.phase
    }

    /// Await `future`, abandoning it if cleanup priority is requested.
    pub async fn until_cleanup_priority<F: Future>(
        self,
        future: F,
    ) -> CleanupInterrupted<F::Output> {
        tokio::pin!(future);
        tokio::select! {
            biased;
            output = &mut future => CleanupInterrupted::Completed(output),
            _ = self.shutdown.cleanup_priority.cancelled() => CleanupInterrupted::Abandoned,
        }
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
    let started = trigger
        .cleanup_started
        .get()
        .expect("cleanup priority start");
    let mut budget = trigger.cleanup_budget.current();
    eprintln!("{}", cleanup_wait_message(budget));
    let cleanup_deadline = tokio::time::sleep_until(*started + budget.total());
    tokio::pin!(cleanup_deadline);
    let mut awaiting_publication = true;

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
            _ = trigger.cleanup_budget.published.cancelled(), if awaiting_publication => {
                awaiting_publication = false;
                let latest = trigger.cleanup_budget.current();
                if latest != budget {
                    budget = latest;
                    cleanup_deadline.as_mut().reset(*started + budget.total());
                    eprintln!("{}", cleanup_wait_message(budget));
                }
            }
            _ = &mut cleanup_deadline => {
                // Publication can race the timer becoming ready on another thread.
                let latest = trigger.cleanup_budget.current();
                if latest != budget {
                    budget = latest;
                    cleanup_deadline.as_mut().reset(*started + budget.total());
                    continue;
                }
                eprintln!(
                    "Cleanup deadline expired after {}; exiting.",
                    duration_for_log(budget.total())
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

fn cleanup_wait_message(budget: CleanupBudget) -> String {
    let reason = match budget.reason {
        Some(CleanupWaitReason::GitHubCommits) => {
            " (GitHub backend: each state or lock write is a separate commit)"
        }
        None => "",
    };
    format!(
        "Waiting up to {} for state and lock cleanup{reason}.",
        duration_for_log(budget.total())
    )
}

pub fn duration_for_log(duration: Duration) -> String {
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

    /// Wait for cleanup-priority mode to be requested in shutdown tests.
    pub async fn cleanup_priority_requested(token: &ShutdownToken) {
        token.cleanup_priority.cancelled().await;
    }

    /// Return whether two test tokens observe the same shutdown channel.
    pub fn same_shutdown_channel(left: &ShutdownToken, right: &ShutdownToken) -> bool {
        std::sync::Arc::ptr_eq(&left.test_channel_identity, &right.test_channel_identity)
    }

    impl TestShutdownTrigger {
        pub fn request_graceful_shutdown(&self) {
            self.0.request_graceful_shutdown();
        }

        pub fn prioritize_cleanup(&self) {
            self.0.prioritize_cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::cleanup_priority_requested;
    use super::*;

    #[test]
    fn per_backend_cleanup_limits_and_publication_are_validated() {
        let budget = CleanupBudget::new(Duration::from_secs(5), Duration::from_secs(8));
        assert_eq!(budget.state_flush(), Duration::from_secs(5));
        assert_eq!(budget.total(), Duration::from_secs(8));
        for (flush, total) in [(0, 2), (1, 0), (2, 2), (3, 2)] {
            assert!(
                std::panic::catch_unwind(|| CleanupBudget::new(
                    Duration::from_secs(flush),
                    Duration::from_secs(total)
                ))
                .is_err()
            );
        }
        let token = ShutdownToken::running();
        let (_, child) = token.with_graceful_shutdown_trigger();
        child.publish_cleanup_budget(budget).unwrap();
        // Resolving another handle cannot replace the command's published limits.
        assert!(
            token
                .publish_cleanup_budget(CleanupBudget::TWO_SECOND)
                .is_err()
        );
        assert_eq!(token.cleanup_budget(), budget);
    }

    #[test]
    fn cleanup_wait_messages_use_the_published_budget_and_typed_reason() {
        assert_eq!(
            cleanup_wait_message(CleanupBudget::TWO_SECOND),
            "Waiting up to 2 seconds for state and lock cleanup."
        );
        let github = CleanupBudget::new(Duration::from_secs(5), Duration::from_secs(8))
            .with_reason(CleanupWaitReason::GitHubCommits);
        assert_eq!(
            cleanup_wait_message(github),
            "Waiting up to 8 seconds for state and lock cleanup (GitHub backend: each state or lock write is a separate commit)."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_rechecks_budget_published_after_cleanup_priority() {
        struct NoEvents;
        impl ShutdownEvents for NoEvents {
            async fn recv(&mut self) -> Option<ShutdownSignal> {
                std::future::pending().await
            }
        }
        struct Exit;
        impl ExitProcess for Exit {
            fn exit(&self, code: i32) {
                assert_eq!(code, 130);
            }
        }
        let github = CleanupBudget::new(Duration::from_secs(5), Duration::from_secs(8));
        for (budget, publish_after) in [
            (CleanupBudget::TWO_SECOND, None),
            (github, Some(Duration::ZERO)),
            (github, Some(Duration::from_secs(1))),
            // A late publication must also shorten the default timer correctly.
            (
                CleanupBudget::new(Duration::from_millis(200), Duration::from_millis(500)),
                Some(Duration::from_millis(100)),
            ),
        ] {
            let (trigger, token) = shutdown_channel();
            if publish_after == Some(Duration::ZERO) {
                token.publish_cleanup_budget(budget).unwrap();
            }
            trigger.prioritize_cleanup();
            let started = tokio::time::Instant::now();
            let command = async move {
                if let Some(delay) = publish_after {
                    tokio::time::sleep(delay).await;
                    token.publish_cleanup_budget(budget).unwrap();
                }
                std::future::pending::<()>().await;
            };
            tokio::pin!(command);
            let result =
                finish_cleanup_before_exit(&mut command, &mut NoEvents, true, &trigger, &Exit)
                    .await;
            assert!(matches!(
                result,
                Supervised::ExitRequested(ShutdownExitReason::DeadlineExpired)
            ));
            assert_eq!(started.elapsed(), budget.total());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn backend_flush_cutoff_starts_only_at_cleanup_priority_and_survives_child_tokens() {
        let (trigger, token) = shutdown_channel();
        let (_, child) = token.with_graceful_shutdown_trigger();
        let budget = CleanupBudget::new(Duration::from_millis(400), Duration::from_secs(2));
        trigger.request_graceful_shutdown();
        tokio::select! {
            biased;
            _ = child.state_flush_budget_expired(budget) => panic!("graceful shutdown must allow draining"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }

        trigger.prioritize_cleanup();
        tokio::time::advance(Duration::from_millis(300)).await;
        trigger.prioritize_cleanup();
        let start = tokio::time::Instant::now();
        child.state_flush_budget_expired(budget).await;
        assert_eq!(start.elapsed(), Duration::from_millis(100));
    }

    #[test]
    fn cleanup_deadline_fits_the_github_actions_sigterm_window() {
        let github_actions_term_to_kill = Duration::from_millis(2_500);

        assert!(
            CLEANUP_DEADLINE < github_actions_term_to_kill,
            "cleanup must finish before GitHub Actions sends SIGKILL 2.5s after SIGTERM"
        );
        assert_eq!(duration_for_log(CLEANUP_DEADLINE), "2 seconds");
    }

    #[test]
    fn operation_local_trigger_cancels_only_the_derived_token() {
        let root = ShutdownToken::running();
        let (trigger, derived) = root.with_graceful_shutdown_trigger();

        trigger.request_graceful_shutdown();

        assert_eq!(derived.phase(), ShutdownPhase::Graceful);
        assert_eq!(root.phase(), ShutdownPhase::Running);
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
        cleanup_priority_requested(&token).await;
    }

    #[test]
    fn cleanup_aware_loop_step_exposes_only_non_cleanup_phases() {
        let (trigger, token) = shutdown_channel();
        let cleanup_loop = token.cleanup_aware_loop();

        let LoopStep::Continue(wait) = cleanup_loop.step() else {
            panic!("running loop must continue");
        };
        assert_eq!(wait.phase(), LoopShutdownPhase::Running);

        trigger.request_graceful_shutdown();
        let LoopStep::Continue(wait) = cleanup_loop.step() else {
            panic!("graceful loop must continue draining in-flight work");
        };
        assert_eq!(wait.phase(), LoopShutdownPhase::Graceful);

        trigger.prioritize_cleanup();
        assert!(matches!(cleanup_loop.step(), LoopStep::Abandon));
    }

    #[tokio::test]
    async fn cleanup_wait_has_named_completed_and_abandoned_outcomes() {
        let (trigger, token) = shutdown_channel();
        let cleanup_loop = token.cleanup_aware_loop();
        let LoopStep::Continue(wait) = cleanup_loop.step() else {
            panic!("running loop must continue");
        };
        assert!(matches!(
            wait.until_cleanup_priority(async { 42 }).await,
            CleanupInterrupted::Completed(42)
        ));

        let LoopStep::Continue(wait) = cleanup_loop.step() else {
            panic!("running loop must continue");
        };
        trigger.prioritize_cleanup();
        assert!(matches!(
            wait.until_cleanup_priority(std::future::pending::<()>())
                .await,
            CleanupInterrupted::Abandoned
        ));
    }
}
