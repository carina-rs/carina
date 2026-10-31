mod host_pattern;
pub mod types;

pub use host_pattern::{HostPattern, HostPatternError};

use std::future::Future;
use std::time::{Duration, Instant};

/// Protocol version for host-plugin communication.
/// Increment when making breaking changes to the protocol types or methods.
pub const PROTOCOL_VERSION: u32 = 15;

/// Oldest provider protocol version this host can load safely.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 15;

/// Default total budget for a provider operation that returns pending.
pub const DEFAULT_OPERATION_BUDGET: Duration = Duration::from_secs(15 * 60);

/// The operation whose schema budget should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Create,
    Update,
    Delete,
}

/// Result of one step in a host-driven provider operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationStep<D, V, P> {
    Done(D),
    Verdict(V),
    Pending { pending: P, poll_after_secs: u32 },
}

/// Result of driving a pending provider operation within its total budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOutcome<D, V, P> {
    Done(D),
    Verdict(V),
    OverBudget { last_pending: P },
}

/// Failure to call the provider while driving an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveError<E, P> {
    Initial(E),
    Poll { error: E, last_pending: P },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextPoll {
    Sleep(Duration),
    OverBudget,
}

/// Clamp a provider-requested poll delay to the protocol bounds and deadline.
///
/// A sleep equal to `remaining` is intentional: callers perform one final
/// poll at the deadline, then report over-budget if that poll is still pending.
fn next_poll(poll_after_secs: u32, remaining: Duration) -> NextPoll {
    if remaining.is_zero() {
        return NextPoll::OverBudget;
    }

    let requested = Duration::from_secs(u64::from(poll_after_secs.clamp(1, 300)));
    NextPoll::Sleep(requested.min(remaining))
}

/// Drive an operation from its initial step through any requested polls.
///
/// The total budget starts before `initial` is polled. Provider-requested
/// delays are clamped to 1–300 seconds and never sleep past the budget. When a
/// sleep reaches the deadline, the driver performs one final poll; only a
/// further pending result becomes [`DriveOutcome::OverBudget`]. The caller
/// supplies the sleep future so this crate stays independent of an async
/// runtime.
pub async fn drive_pending<D, V, P, E, Initial, Poll, PollFuture, Sleep, SleepFuture>(
    budget: Duration,
    initial: Initial,
    poll: Poll,
    sleep: Sleep,
) -> Result<DriveOutcome<D, V, P>, DriveError<E, P>>
where
    P: Clone,
    Initial: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Poll: FnMut(P) -> PollFuture,
    PollFuture: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
{
    drive_pending_with_clock(budget, initial, poll, sleep, Instant::now).await
}

/// Drive a pending operation whose boundary converts every call failure into
/// a typed verdict before returning. Unlike [`drive_pending`], this linear
/// variant never clones the pending capability merely to retain an error
/// fallback.
pub async fn drive_pending_infallible<D, V, P, Initial, Poll, PollFuture, Sleep, SleepFuture>(
    budget: Duration,
    initial: Initial,
    mut poll: Poll,
    mut sleep: Sleep,
) -> DriveOutcome<D, V, P>
where
    Initial: Future<Output = OperationStep<D, V, P>>,
    Poll: FnMut(P) -> PollFuture,
    PollFuture: Future<Output = OperationStep<D, V, P>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
{
    let started = Instant::now();
    let mut step = initial.await;
    loop {
        let (pending, poll_after_secs) = match step {
            OperationStep::Done(done) => return DriveOutcome::Done(done),
            OperationStep::Verdict(verdict) => return DriveOutcome::Verdict(verdict),
            OperationStep::Pending {
                pending,
                poll_after_secs,
            } => (pending, poll_after_secs),
        };
        let elapsed = Instant::now().saturating_duration_since(started);
        let remaining = budget.saturating_sub(elapsed);
        match next_poll(poll_after_secs, remaining) {
            NextPoll::OverBudget => {
                return DriveOutcome::OverBudget {
                    last_pending: pending,
                };
            }
            NextPoll::Sleep(delay) => sleep(delay).await,
        }
        step = poll(pending).await;
    }
}

async fn drive_pending_with_clock<D, V, P, E, Initial, Poll, PollFuture, Sleep, SleepFuture, Now>(
    budget: Duration,
    initial: Initial,
    mut poll: Poll,
    mut sleep: Sleep,
    mut now: Now,
) -> Result<DriveOutcome<D, V, P>, DriveError<E, P>>
where
    P: Clone,
    Initial: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Poll: FnMut(P) -> PollFuture,
    PollFuture: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Now: FnMut() -> Instant,
{
    let started = now();
    let mut step = initial.await.map_err(DriveError::Initial)?;
    loop {
        let (pending, poll_after_secs) = match step {
            OperationStep::Done(done) => return Ok(DriveOutcome::Done(done)),
            OperationStep::Verdict(verdict) => return Ok(DriveOutcome::Verdict(verdict)),
            OperationStep::Pending {
                pending,
                poll_after_secs,
            } => (pending, poll_after_secs),
        };
        let elapsed = now().saturating_duration_since(started);
        let remaining = budget.saturating_sub(elapsed);
        match next_poll(poll_after_secs, remaining) {
            NextPoll::OverBudget => {
                return Ok(DriveOutcome::OverBudget {
                    last_pending: pending,
                });
            }
            NextPoll::Sleep(delay) => sleep(delay).await,
        }
        let last_pending = pending.clone();
        step = poll(pending).await.map_err(|error| DriveError::Poll {
            error,
            last_pending,
        })?;
    }
}

/// Host-reserved object key for a typed redacted-secret digest.
pub const PERSISTED_SECRET_KEY: &str = "$secret";

pub use types::*;

#[cfg(test)]
mod operation_poll_tests {
    use std::cell::{Cell, RefCell};
    use std::future::{Future, ready};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use super::{DriveOutcome, NextPoll, OperationStep, drive_pending_with_clock, next_poll};

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test futures must complete synchronously"),
        }
    }

    #[test]
    fn next_poll_clamps_requested_delay_to_bounds() {
        assert_eq!(
            next_poll(0, Duration::from_secs(600)),
            NextPoll::Sleep(Duration::from_secs(1))
        );
        assert_eq!(
            next_poll(301, Duration::from_secs(600)),
            NextPoll::Sleep(Duration::from_secs(300))
        );
    }

    #[test]
    fn next_poll_never_sleeps_past_deadline() {
        assert_eq!(
            next_poll(300, Duration::from_millis(750)),
            NextPoll::Sleep(Duration::from_millis(750))
        );
    }

    #[test]
    fn next_poll_allows_sleep_to_deadline_for_one_final_poll() {
        assert_eq!(
            next_poll(5, Duration::from_secs(5)),
            NextPoll::Sleep(Duration::from_secs(5))
        );
        assert_eq!(next_poll(5, Duration::ZERO), NextPoll::OverBudget);
    }

    #[test]
    fn drive_pending_budget_starts_before_initial_step() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(1),
            async {
                elapsed.set(Duration::from_secs(2));
                Ok::<_, ()>(OperationStep::<(), (), ()>::Pending {
                    pending: (),
                    poll_after_secs: 1,
                })
            },
            |_| {
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::<(), (), ()>::Done(())))
            },
            |_| ready(()),
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(result, DriveOutcome::OverBudget { last_pending: () });
        assert_eq!(polls.get(), 0);
    }

    #[test]
    fn drive_pending_performs_one_final_poll_at_deadline() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let sleeps = RefCell::new(Vec::new());
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            ready(Ok::<_, ()>(OperationStep::<&str, (), &str>::Pending {
                pending: "token",
                poll_after_secs: 30,
            })),
            |pending| {
                assert_eq!(pending, "token");
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::<&str, (), &str>::Done("settled")))
            },
            |delay| {
                sleeps.borrow_mut().push(delay);
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(result, DriveOutcome::Done("settled"));
        assert_eq!(polls.get(), 1);
        assert_eq!(*sleeps.borrow(), vec![Duration::from_secs(5)]);
    }

    #[test]
    fn drive_pending_returns_verdict_separately_from_call_error() {
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            ready(Ok::<_, &'static str>(OperationStep::Verdict("discarded"))),
            |_pending: String| ready(Ok(OperationStep::Done("settled"))),
            |_| ready(()),
            std::time::Instant::now,
        ));

        assert_eq!(result, Ok(DriveOutcome::Verdict("discarded")));
    }

    #[test]
    fn drive_pending_initial_error_has_no_pending_record() {
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            ready(Err::<OperationStep<(), (), String>, _>("initial")),
            |_pending| ready(Ok(OperationStep::Done(()))),
            |_| ready(()),
            std::time::Instant::now,
        ));

        assert_eq!(result, Err(super::DriveError::Initial("initial")));
    }

    #[test]
    fn drive_pending_poll_error_preserves_last_pending() {
        let pending = String::from("provider-token");
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            ready(Ok::<_, &'static str>(OperationStep::<(), (), _>::Pending {
                pending: pending.clone(),
                poll_after_secs: 1,
            })),
            |_pending| ready(Err("poll")),
            |_| ready(()),
            std::time::Instant::now,
        ));

        assert_eq!(
            result,
            Err(super::DriveError::Poll {
                error: "poll",
                last_pending: pending,
            })
        );
    }

    #[test]
    fn drive_pending_over_budget_preserves_last_pending() {
        let pending = String::from("provider-token");
        let clock_epoch = std::time::Instant::now();
        let result = block_on_ready(drive_pending_with_clock(
            Duration::ZERO,
            ready(Ok::<_, ()>(OperationStep::<(), (), _>::Pending {
                pending: pending.clone(),
                poll_after_secs: 1,
            })),
            |_pending| ready(Ok(OperationStep::Done(()))),
            |_| ready(()),
            || clock_epoch,
        ));

        assert_eq!(
            result,
            Ok(DriveOutcome::OverBudget {
                last_pending: pending,
            })
        );
    }
}
