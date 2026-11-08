use carina_core::provider_text::ProviderCause;

pub fn matches(cause: ProviderCause<'_>) -> bool {
    cause.matches_for_test::<std::io::Error>(|_| true)
}

// The partial-reason fixture must not grant native plaintext encoding.
pub fn partial_wire(partial: carina_core::provider::PartialReadDiagnostic) {
    let _ = partial.to_wire();
}
pub fn partial_matches(partial: carina_core::provider::PartialReadDiagnostic) -> bool {
    partial.diagnostic().as_cause().matches_for_test::<std::io::Error>(|_| true)
}

pub fn error_round_trip(error: carina_core::provider::ProviderError) {
    let _ = error.round_trip();
}
