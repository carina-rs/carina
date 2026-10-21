pub mod jsonrpc;
pub mod methods;
pub mod types;

use std::time::Duration;

/// Protocol version for host-plugin communication.
/// Increment when making breaking changes to the protocol types or methods.
pub const PROTOCOL_VERSION: u32 = 8;

/// Oldest provider protocol version this host can load safely.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 8;

/// Default total budget for a provider operation that returns pending.
pub const DEFAULT_OPERATION_BUDGET: Duration = Duration::from_secs(15 * 60);

/// The operation whose schema budget should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Create,
    Update,
    Delete,
}

/// Host action for a provider-requested poll delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextPoll {
    Sleep(Duration),
    OverBudget,
}

/// Clamp a provider-requested poll delay to the protocol bounds and deadline.
///
/// A sleep equal to `remaining` is intentional: callers perform one final
/// poll at the deadline, then report over-budget if that poll is still pending.
pub fn next_poll(poll_after_secs: u32, remaining: Duration) -> NextPoll {
    if remaining.is_zero() {
        return NextPoll::OverBudget;
    }

    let requested = Duration::from_secs(u64::from(poll_after_secs.clamp(1, 300)));
    NextPoll::Sleep(requested.min(remaining))
}

/// Host-reserved object key for a typed redacted-secret digest.
pub const PERSISTED_SECRET_KEY: &str = "$secret";

pub use jsonrpc::*;
pub use methods::*;
pub use types::*;

#[cfg(test)]
mod operation_poll_tests {
    use std::time::Duration;

    use super::{NextPoll, next_poll};

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
}
