//! Native authoring carriers; only SDK encoders produce protocol diagnostics.
use crate::types::{AppliedPersistedState, PartialReadDiagnostic, ProviderError};

#[derive(Debug, Clone)]
pub enum ReferenceOutcome {
    Resolved(String),
    NotFound(Box<ProviderError>),
}
#[derive(Debug, Clone)]
pub enum ReferenceError {
    AccessDenied(Box<ProviderError>),
    Transient(Box<ProviderError>),
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
