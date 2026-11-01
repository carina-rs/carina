mod host_pattern;
pub mod types;

pub use host_pattern::{HostPattern, HostPatternError};

use std::future::Future;
use std::time::{Duration, Instant};

/// Protocol version for host-plugin communication.
/// Increment when making breaking changes to the protocol types or methods.
pub const PROTOCOL_VERSION: u32 = 17;

/// Oldest provider protocol version this host can load safely.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = PROTOCOL_VERSION;

/// Default total budget for a provider operation that returns pending.
pub const DEFAULT_OPERATION_BUDGET: Duration = Duration::from_secs(15 * 60);

/// Default budget for a post-apply read when the schema does not set one.
pub const DEFAULT_READ_BACK_BUDGET: Duration = Duration::from_secs(2 * 60);

/// The part of a provider operation represented by a pending result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingPhase {
    Operation,
    ReadBack,
}

/// Whether a provider poll is the one attempt at the active deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PollFinality {
    Continue,
    FinalAttempt,
}

/// Host-owned scheduling information supplied to a provider poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PollRequest {
    pub finality: PollFinality,
}

/// Why a completed post-mutation read stopped being polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadBackEnd {
    /// No host-final read-back attempt produced this result.
    ProviderReported,
    /// The provider returned a partial result to the host's final read-back poll.
    HostDeadline,
}

/// A completed operation together with host-owned polling provenance.
///
/// Construction is private so `HostDeadline` can enter core only through a
/// result returned by [`drive_pending`] or [`drive_pending_infallible`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedOperation<D> {
    output: D,
    read_back_end: ReadBackEnd,
}

impl<D> CompletedOperation<D> {
    fn new(output: D, read_back_end: ReadBackEnd) -> Self {
        Self {
            output,
            read_back_end,
        }
    }

    pub fn read_back_end(&self) -> ReadBackEnd {
        self.read_back_end
    }

    pub fn into_parts(self) -> (D, ReadBackEnd) {
        (self.output, self.read_back_end)
    }

    pub fn into_output(self) -> D {
        self.output
    }
}

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
    Pending {
        pending: P,
        poll_after_secs: u32,
        phase: PendingPhase,
    },
}

/// Result of driving a pending provider operation within its total budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOutcome<D, V, P> {
    Done(CompletedOperation<D>),
    Verdict(V),
    OverBudget {
        last_pending: P,
    },
    /// A provider returned any pending continuation after a final attempt sent
    /// to a read-back continuation, or regressed from read-back to operation.
    ProtocolViolation {
        last_pending: P,
    },
}

/// Failure to call the provider while driving an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveError<E, P> {
    Initial(E),
    Poll { error: E, last_pending: P },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NextPoll {
    delay: Duration,
    finality: PollFinality,
}

/// Clamp a provider-requested poll delay to the protocol bounds and deadline.
///
/// A sleep equal to `remaining` is intentional: callers perform one final
/// poll at the deadline, then report over-budget if that poll is still pending.
fn next_poll(poll_after_secs: u32, remaining: Duration) -> NextPoll {
    let requested = Duration::from_secs(u64::from(poll_after_secs.clamp(1, 300)));
    let delay = requested.min(remaining);
    NextPoll {
        delay,
        finality: if delay == remaining {
            PollFinality::FinalAttempt
        } else {
            PollFinality::Continue
        },
    }
}

#[derive(Debug)]
struct PollDeadlines {
    operation_budget: Duration,
    read_back_budget: Duration,
    read_back_deadline: Option<Duration>,
}

impl PollDeadlines {
    fn new(operation_budget: Duration, read_back_budget: Duration) -> Self {
        Self {
            operation_budget,
            read_back_budget,
            read_back_deadline: None,
        }
    }

    fn next(&mut self, phase: PendingPhase, poll_after_secs: u32, elapsed: Duration) -> NextPoll {
        if phase == PendingPhase::ReadBack && self.read_back_deadline.is_none() {
            self.read_back_deadline = Some(
                elapsed
                    .saturating_add(self.read_back_budget)
                    .min(self.operation_budget),
            );
        }
        let active_deadline = self.read_back_deadline.unwrap_or(self.operation_budget);
        next_poll(poll_after_secs, active_deadline.saturating_sub(elapsed))
    }
}

/// Drive an operation from its initial step through any requested polls.
///
/// The total budget starts before `initial` is polled. Provider-requested
/// delays are clamped to 1–300 seconds and never sleep past the budget. When a
/// sleep reaches the deadline, the driver performs one final poll. A further
/// pending result after a final attempt sent to an operation-phase
/// continuation becomes [`DriveOutcome::OverBudget`]. Any pending result after
/// a final attempt sent to a read-back continuation is a
/// [`DriveOutcome::ProtocolViolation`].
/// The caller supplies the sleep future so this crate stays independent of an
/// async runtime.
pub async fn drive_pending<D, V, P, E, Initial, Poll, PollFuture, Sleep, SleepFuture>(
    operation_budget: Duration,
    read_back_budget: Duration,
    initial: Initial,
    poll: Poll,
    sleep: Sleep,
) -> Result<DriveOutcome<D, V, P>, DriveError<E, P>>
where
    P: Clone,
    Initial: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Poll: FnMut(P, PollFinality) -> PollFuture,
    PollFuture: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
{
    drive_pending_with_clock(
        operation_budget,
        read_back_budget,
        initial,
        poll,
        sleep,
        Instant::now,
    )
    .await
}

/// Drive a pending operation whose boundary converts every call failure into
/// a typed verdict before returning. Unlike [`drive_pending`], this linear
/// variant never clones the pending capability merely to retain an error
/// fallback.
pub async fn drive_pending_infallible<D, V, P, Initial, Poll, PollFuture, Sleep, SleepFuture>(
    operation_budget: Duration,
    read_back_budget: Duration,
    initial: Initial,
    poll: Poll,
    sleep: Sleep,
) -> DriveOutcome<D, V, P>
where
    Initial: Future<Output = OperationStep<D, V, P>>,
    Poll: FnMut(P, PollFinality) -> PollFuture,
    PollFuture: Future<Output = OperationStep<D, V, P>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
{
    drive_pending_infallible_with_clock(
        operation_budget,
        read_back_budget,
        initial,
        poll,
        sleep,
        Instant::now,
    )
    .await
}

async fn drive_pending_with_clock<D, V, P, E, Initial, Poll, PollFuture, Sleep, SleepFuture, Now>(
    operation_budget: Duration,
    read_back_budget: Duration,
    initial: Initial,
    poll: Poll,
    sleep: Sleep,
    now: Now,
) -> Result<DriveOutcome<D, V, P>, DriveError<E, P>>
where
    P: Clone,
    Initial: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Poll: FnMut(P, PollFinality) -> PollFuture,
    PollFuture: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Now: FnMut() -> Instant,
{
    drive_pending_with_clock_and_retention(
        operation_budget,
        read_back_budget,
        async move { initial.await.map_err(DriveError::Initial) },
        poll,
        sleep,
        now,
        PreservePendingOnError,
    )
    .await
}

async fn drive_pending_infallible_with_clock<
    D,
    V,
    P,
    Initial,
    Poll,
    PollFuture,
    Sleep,
    SleepFuture,
    Now,
>(
    operation_budget: Duration,
    read_back_budget: Duration,
    initial: Initial,
    mut poll: Poll,
    sleep: Sleep,
    now: Now,
) -> DriveOutcome<D, V, P>
where
    Initial: Future<Output = OperationStep<D, V, P>>,
    Poll: FnMut(P, PollFinality) -> PollFuture,
    PollFuture: Future<Output = OperationStep<D, V, P>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Now: FnMut() -> Instant,
{
    let outcome = drive_pending_with_clock_and_retention(
        operation_budget,
        read_back_budget,
        async move { Ok::<_, DriveError<std::convert::Infallible, P>>(initial.await) },
        |pending, finality| {
            let future = poll(pending, finality);
            async move { Ok::<_, std::convert::Infallible>(future.await) }
        },
        sleep,
        now,
        NoPendingNeededOnError,
    )
    .await;
    match outcome {
        Ok(outcome) => outcome,
        Err(DriveError::Initial(never)) => match never {},
        Err(DriveError::Poll {
            error: never,
            last_pending: _last_pending,
        }) => match never {},
    }
}

trait PendingErrorRetention<P, E> {
    type Retained;

    fn retain(&mut self, pending: &P) -> Self::Retained;
    fn poll_error(&mut self, error: E, retained: Self::Retained) -> DriveError<E, P>;
}

struct PreservePendingOnError;

impl<P: Clone, E> PendingErrorRetention<P, E> for PreservePendingOnError {
    type Retained = P;

    fn retain(&mut self, pending: &P) -> Self::Retained {
        pending.clone()
    }

    fn poll_error(&mut self, error: E, last_pending: Self::Retained) -> DriveError<E, P> {
        DriveError::Poll {
            error,
            last_pending,
        }
    }
}

struct NoPendingNeededOnError;

impl<P> PendingErrorRetention<P, std::convert::Infallible> for NoPendingNeededOnError {
    type Retained = ();

    fn retain(&mut self, _pending: &P) -> Self::Retained {}

    fn poll_error(
        &mut self,
        error: std::convert::Infallible,
        _retained: Self::Retained,
    ) -> DriveError<std::convert::Infallible, P> {
        match error {}
    }
}

async fn drive_pending_with_clock_and_retention<
    D,
    V,
    P,
    E,
    Initial,
    Poll,
    PollFuture,
    Sleep,
    SleepFuture,
    Now,
    Retention,
>(
    operation_budget: Duration,
    read_back_budget: Duration,
    initial: Initial,
    mut poll: Poll,
    mut sleep: Sleep,
    mut now: Now,
    mut retention: Retention,
) -> Result<DriveOutcome<D, V, P>, DriveError<E, P>>
where
    Initial: Future<Output = Result<OperationStep<D, V, P>, DriveError<E, P>>>,
    Poll: FnMut(P, PollFinality) -> PollFuture,
    PollFuture: Future<Output = Result<OperationStep<D, V, P>, E>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Now: FnMut() -> Instant,
    Retention: PendingErrorRetention<P, E>,
{
    let started = now();
    let mut deadlines = PollDeadlines::new(operation_budget, read_back_budget);
    let mut final_attempt_phase = None;
    let mut step = initial.await?;
    loop {
        let (pending, poll_after_secs, phase) = match step {
            OperationStep::Done(done) => {
                let read_back_end = if final_attempt_phase == Some(PendingPhase::ReadBack) {
                    ReadBackEnd::HostDeadline
                } else {
                    ReadBackEnd::ProviderReported
                };
                return Ok(DriveOutcome::Done(CompletedOperation::new(
                    done,
                    read_back_end,
                )));
            }
            OperationStep::Verdict(verdict) => return Ok(DriveOutcome::Verdict(verdict)),
            OperationStep::Pending {
                pending,
                poll_after_secs,
                phase,
            } => (pending, poll_after_secs, phase),
        };
        if deadlines.read_back_deadline.is_some() && phase == PendingPhase::Operation {
            return Ok(DriveOutcome::ProtocolViolation {
                last_pending: pending,
            });
        }
        if let Some(final_attempt_phase) = final_attempt_phase {
            return Ok(match final_attempt_phase {
                PendingPhase::Operation => DriveOutcome::OverBudget {
                    last_pending: pending,
                },
                PendingPhase::ReadBack => DriveOutcome::ProtocolViolation {
                    last_pending: pending,
                },
            });
        }
        let elapsed = now().saturating_duration_since(started);
        let next = deadlines.next(phase, poll_after_secs, elapsed);
        if !next.delay.is_zero() {
            sleep(next.delay).await;
        }
        let retained = retention.retain(&pending);
        step = poll(pending, next.finality)
            .await
            .map_err(|error| retention.poll_error(error, retained))?;
        if next.finality == PollFinality::FinalAttempt {
            final_attempt_phase = Some(phase);
        }
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

    use super::{
        CompletedOperation, DriveOutcome, NextPoll, OperationStep, PendingPhase, PollFinality,
        ReadBackEnd, drive_pending_infallible_with_clock, drive_pending_with_clock, next_poll,
    };

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
            NextPoll {
                delay: Duration::from_secs(1),
                finality: PollFinality::Continue,
            }
        );
        assert_eq!(
            next_poll(301, Duration::from_secs(600)),
            NextPoll {
                delay: Duration::from_secs(300),
                finality: PollFinality::Continue,
            }
        );
    }

    #[test]
    fn next_poll_never_sleeps_past_deadline() {
        assert_eq!(
            next_poll(300, Duration::from_millis(750)),
            NextPoll {
                delay: Duration::from_millis(750),
                finality: PollFinality::FinalAttempt,
            }
        );
    }

    #[test]
    fn next_poll_allows_sleep_to_deadline_for_one_final_poll() {
        assert_eq!(
            next_poll(5, Duration::from_secs(5)),
            NextPoll {
                delay: Duration::from_secs(5),
                finality: PollFinality::FinalAttempt,
            }
        );
        assert_eq!(
            next_poll(5, Duration::ZERO),
            NextPoll {
                delay: Duration::ZERO,
                finality: PollFinality::FinalAttempt,
            }
        );
    }

    #[test]
    fn drive_pending_budget_starts_before_initial_step() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(1),
            Duration::from_secs(30),
            async {
                elapsed.set(Duration::from_secs(2));
                Ok::<_, ()>(OperationStep::<(), (), ()>::Pending {
                    pending: (),
                    poll_after_secs: 1,
                    phase: PendingPhase::Operation,
                })
            },
            |_, finality| {
                assert_eq!(finality, PollFinality::FinalAttempt);
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::<(), (), ()>::Done(())))
            },
            |_| ready(()),
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::Done(CompletedOperation::new((), ReadBackEnd::ProviderReported,))
        );
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn drive_pending_performs_one_final_poll_at_deadline() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let sleeps = RefCell::new(Vec::new());
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            Duration::from_secs(30),
            ready(Ok::<_, ()>(OperationStep::<&str, (), &str>::Pending {
                pending: "token",
                poll_after_secs: 30,
                phase: PendingPhase::Operation,
            })),
            |pending, finality| {
                assert_eq!(pending, "token");
                assert_eq!(finality, PollFinality::FinalAttempt);
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

        assert_eq!(
            result,
            DriveOutcome::Done(CompletedOperation::new(
                "settled",
                ReadBackEnd::ProviderReported,
            ))
        );
        assert_eq!(polls.get(), 1);
        assert_eq!(*sleeps.borrow(), vec![Duration::from_secs(5)]);
    }

    #[test]
    fn drive_pending_returns_verdict_separately_from_call_error() {
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            Duration::from_secs(30),
            ready(Ok::<_, &'static str>(OperationStep::Verdict("discarded"))),
            |_pending: String, _finality| ready(Ok(OperationStep::Done("settled"))),
            |_| ready(()),
            std::time::Instant::now,
        ));

        assert_eq!(result, Ok(DriveOutcome::Verdict("discarded")));
    }

    #[test]
    fn drive_pending_initial_error_has_no_pending_record() {
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            Duration::from_secs(30),
            ready(Err::<OperationStep<(), (), String>, _>("initial")),
            |_pending, _finality| ready(Ok(OperationStep::Done(()))),
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
            Duration::from_secs(30),
            ready(Ok::<_, &'static str>(OperationStep::<(), (), _>::Pending {
                pending: pending.clone(),
                poll_after_secs: 1,
                phase: PendingPhase::Operation,
            })),
            |_pending, _finality| ready(Err("poll")),
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
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::ZERO,
            Duration::from_secs(30),
            ready(Ok::<_, ()>(OperationStep::<(), (), _>::Pending {
                pending: pending.clone(),
                poll_after_secs: 1,
                phase: PendingPhase::Operation,
            })),
            |pending, finality| {
                assert_eq!(finality, PollFinality::FinalAttempt);
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::Pending {
                    pending,
                    poll_after_secs: 1,
                    phase: PendingPhase::Operation,
                }))
            },
            |_| ready(()),
            || clock_epoch,
        ));

        assert_eq!(
            result,
            Ok(DriveOutcome::OverBudget {
                last_pending: pending,
            })
        );
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn read_back_deadline_starts_at_first_read_back_pending() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let sleeps = RefCell::new(Vec::new());
        let finalities = RefCell::new(Vec::new());
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(20),
            Duration::from_secs(5),
            ready(Ok::<_, ()>(OperationStep::<&str, (), &str>::Pending {
                pending: "operation",
                poll_after_secs: 2,
                phase: super::PendingPhase::Operation,
            })),
            |pending, finality| {
                finalities.borrow_mut().push(finality);
                let poll = polls.get();
                polls.set(poll + 1);
                ready(Ok(if poll == 0 {
                    assert_eq!(pending, "operation");
                    OperationStep::Pending {
                        pending: "read-back",
                        poll_after_secs: 300,
                        phase: super::PendingPhase::ReadBack,
                    }
                } else {
                    assert_eq!(pending, "read-back");
                    OperationStep::Done("settled")
                }))
            },
            |delay| {
                sleeps.borrow_mut().push(delay);
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::Done(CompletedOperation::new(
                "settled",
                ReadBackEnd::HostDeadline,
            ))
        );
        assert_eq!(
            *sleeps.borrow(),
            vec![Duration::from_secs(2), Duration::from_secs(5)]
        );
        assert_eq!(
            *finalities.borrow(),
            vec![
                super::PollFinality::Continue,
                super::PollFinality::FinalAttempt,
            ]
        );
    }

    #[test]
    fn read_back_deadline_is_clipped_to_operation_deadline() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let sleeps = RefCell::new(Vec::new());
        let finalities = RefCell::new(Vec::new());
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            Duration::from_secs(10),
            ready(Ok::<_, ()>(OperationStep::<&str, (), &str>::Pending {
                pending: "operation",
                poll_after_secs: 4,
                phase: super::PendingPhase::Operation,
            })),
            |_pending, finality| {
                finalities.borrow_mut().push(finality);
                let poll = polls.get();
                polls.set(poll + 1);
                ready(Ok(if poll == 0 {
                    OperationStep::Pending {
                        pending: "read-back",
                        poll_after_secs: 30,
                        phase: super::PendingPhase::ReadBack,
                    }
                } else {
                    OperationStep::Done("settled")
                }))
            },
            |delay| {
                sleeps.borrow_mut().push(delay);
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::Done(CompletedOperation::new(
                "settled",
                ReadBackEnd::HostDeadline,
            ))
        );
        assert_eq!(
            *sleeps.borrow(),
            vec![Duration::from_secs(4), Duration::from_secs(1)]
        );
        assert_eq!(
            *finalities.borrow(),
            vec![
                super::PollFinality::Continue,
                super::PollFinality::FinalAttempt,
            ]
        );
    }

    #[test]
    fn drive_pending_sends_exactly_one_final_attempt() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let finalities = RefCell::new(Vec::new());
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(5),
            Duration::from_secs(30),
            ready(Ok::<_, ()>(OperationStep::<&str, (), &str>::Pending {
                pending: "token",
                poll_after_secs: 2,
                phase: super::PendingPhase::Operation,
            })),
            |_pending, finality| {
                finalities.borrow_mut().push(finality);
                let poll = polls.get();
                polls.set(poll + 1);
                ready(Ok(if finality == super::PollFinality::FinalAttempt {
                    OperationStep::Done("settled")
                } else {
                    OperationStep::Pending {
                        pending: "token",
                        poll_after_secs: 2,
                        phase: super::PendingPhase::Operation,
                    }
                }))
            },
            |delay| {
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::Done(CompletedOperation::new(
                "settled",
                ReadBackEnd::ProviderReported,
            ))
        );
        assert_eq!(
            finalities
                .borrow()
                .iter()
                .filter(|&&finality| finality == super::PollFinality::FinalAttempt)
                .count(),
            1
        );
    }

    #[test]
    fn pending_read_back_after_final_attempt_is_protocol_violation() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(30),
            Duration::from_secs(3),
            ready(Ok::<_, ()>(OperationStep::<(), (), String>::Pending {
                pending: "known-id:token-1".to_string(),
                poll_after_secs: 30,
                phase: super::PendingPhase::ReadBack,
            })),
            |_pending, finality| {
                assert_eq!(finality, super::PollFinality::FinalAttempt);
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::Pending {
                    pending: "known-id:token-2".to_string(),
                    poll_after_secs: 30,
                    phase: super::PendingPhase::ReadBack,
                }))
            },
            |delay| {
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::ProtocolViolation {
                last_pending: "known-id:token-2".to_string(),
            }
        );
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn read_back_pending_after_operation_final_attempt_is_over_budget() {
        let clock_epoch = std::time::Instant::now();
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::ZERO,
            Duration::from_secs(30),
            ready(Ok::<_, ()>(OperationStep::<(), (), String>::Pending {
                pending: "known-id:operation-token".to_string(),
                poll_after_secs: 1,
                phase: super::PendingPhase::Operation,
            })),
            |_pending, finality| {
                assert_eq!(finality, super::PollFinality::FinalAttempt);
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::Pending {
                    pending: "known-id:read-back-token".to_string(),
                    poll_after_secs: 1,
                    phase: super::PendingPhase::ReadBack,
                }))
            },
            |_| ready(()),
            || clock_epoch,
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::OverBudget {
                last_pending: "known-id:read-back-token".to_string(),
            }
        );
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn operation_phase_after_read_back_is_protocol_violation_and_preserves_pending() {
        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let polls = Cell::new(0);
        let result = block_on_ready(drive_pending_with_clock(
            Duration::from_secs(30),
            Duration::from_secs(3),
            ready(Ok::<_, ()>(OperationStep::<(), (), String>::Pending {
                pending: "known-id:read-back-token".to_string(),
                poll_after_secs: 30,
                phase: super::PendingPhase::ReadBack,
            })),
            |_pending, finality| {
                assert_eq!(finality, super::PollFinality::FinalAttempt);
                polls.set(polls.get() + 1);
                ready(Ok(OperationStep::Pending {
                    pending: "known-id:regressed-operation-token".to_string(),
                    poll_after_secs: 30,
                    phase: super::PendingPhase::Operation,
                }))
            },
            |delay| {
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ))
        .expect("driver should not produce a provider error");

        assert_eq!(
            result,
            DriveOutcome::ProtocolViolation {
                last_pending: "known-id:regressed-operation-token".to_string(),
            }
        );
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn infallible_driver_uses_injected_clock_for_linear_pending_value() {
        #[derive(Debug, PartialEq, Eq)]
        struct LinearPending(&'static str);

        let elapsed = Cell::new(Duration::ZERO);
        let clock_epoch = std::time::Instant::now();
        let result = block_on_ready(drive_pending_infallible_with_clock(
            Duration::from_secs(30),
            Duration::from_secs(4),
            ready(OperationStep::<&str, (), _>::Pending {
                pending: LinearPending("read-back-token"),
                poll_after_secs: 300,
                phase: PendingPhase::ReadBack,
            }),
            |pending, finality| {
                assert_eq!(pending, LinearPending("read-back-token"));
                assert_eq!(finality, PollFinality::FinalAttempt);
                ready(OperationStep::Done("settled"))
            },
            |delay| {
                elapsed.set(elapsed.get() + delay);
                ready(())
            },
            || clock_epoch + elapsed.get(),
        ));

        assert_eq!(
            result,
            DriveOutcome::Done(CompletedOperation::new(
                "settled",
                ReadBackEnd::HostDeadline,
            ))
        );
        assert_eq!(elapsed.get(), Duration::from_secs(4));
    }
}
