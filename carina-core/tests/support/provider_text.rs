#![allow(dead_code)]
use carina_core::provider::*;
use carina_core::provider_text::*;
pub use carina_core::test_utils::provider_text::*;

pub trait InspectProviderCause {
    fn redact(&self, context: &Context) -> String;
}
impl InspectProviderCause for ProviderText {
    fn redact(&self, context: &Context) -> String {
        ProviderError::internal(self.clone())
            .with_secrets_for_test(context)
            .to_string()
    }
}
pub trait InspectDiagnostic {
    fn error(&self) -> ProviderCause<'_>;
    fn normalization_for_test(&self) -> Option<RedactedNormalizationCause<'_>>;
}
impl InspectDiagnostic for HostDiagnostic {
    fn error(&self) -> ProviderCause<'_> {
        self.as_cause()
    }
    fn normalization_for_test(&self) -> Option<RedactedNormalizationCause<'_>> {
        self.normalization()
    }
}

/// Explicitly opaque label for legacy unit probes; grants no Display implementation.
pub trait OpaqueFixtureLabel {
    fn to_string(&self) -> String {
        "<provider text>".into()
    }
}
impl OpaqueFixtureLabel for ProviderError {}
impl OpaqueFixtureLabel for ProviderDiagnostic {}
