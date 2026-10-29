//! Shared replacement/update patch helpers.

use std::time::Duration;

use crate::resource::{DataSource, State};

use super::ProgressInfo;
use super::basic::BasicEffectResult;
use super::wait::WaitOutcome;
use crate::binding_index::SecretPreservingAttributes;

/// Result of executing a single effect.
pub(super) enum SingleEffectResult<S> {
    /// Create/Update/Delete completed (wraps BasicEffectResult)
    Basic(Box<BasicEffectResult>),
    ReadNoOp,
    /// Apply-time data-source read outcome. Successful reads publish
    /// their returned state under the data-source binding before
    /// downstream effects are scheduled.
    Read {
        resource: Box<DataSource<S>>,
        outcome: Result<(SecretPreservingAttributes, State), String>,
        duration: Duration,
        progress: ProgressInfo,
    },
    /// `Effect::Wait` execution outcome. On success carries the
    /// captured target state so the parallel scheduler can register it
    /// under the wait binding for downstream resolution. On failure
    /// carries the wait binding so dependents can be marked failed.
    Wait {
        binding: String,
        target_binding: String,
        outcome: WaitOutcome,
        duration: Duration,
        progress: ProgressInfo,
    },
}
