//! Native authoring carriers; transport encoding flattens their diagnostics.
use crate::types::{AppliedPersistedState, PartialReadDiagnostic, ProviderError};

#[derive(Debug, Clone)]
pub enum ReferenceOutcome {
    Resolved(String),
    NotFound(Box<ProviderError>),
}
/// Reference-resolution failure; retryability comes only from the inner error.
#[derive(Debug, Clone)]
pub enum ReferenceError {
    AccessDenied(Box<ProviderError>),
    /// Includes temporary resolution failures with `Retryability::Transient`.
    InvalidReference(Box<ProviderError>),
}
#[derive(Debug, Clone)]
pub enum UpdateResult {
    Success {
        state: AppliedPersistedState,
    },
    PartialSuccess {
        state: AppliedPersistedState,
        diagnostic: PartialReadDiagnostic,
    },
}
