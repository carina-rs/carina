//! Shared helpers used by `apply` and `destroy` commands.
//!
//! This module groups progress UI, the CLI observer, retry helpers, state
//! write-back logic, and small effect-execution helpers that were previously
//! co-located with the `apply` / `destroy` orchestration code. Splitting them
//! out lets the command files focus on top-level flow and tightens the
//! cohesion of the shared utilities.

#[cfg(test)]
pub(crate) mod cancellation_test_support;
pub(crate) mod effect_execution;
pub(crate) mod finalize;
pub(crate) mod observer;
pub(crate) mod progress;
pub(crate) mod retry;
pub(crate) mod state_writeback;
