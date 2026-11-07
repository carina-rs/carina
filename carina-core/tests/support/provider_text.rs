#![allow(dead_code)]
use carina_core::provider::*;
use carina_core::provider_text::*;
pub use carina_core::test_utils::provider_text::*;

pub trait InspectProviderCause {
    fn redact(&self, context: &Context) -> String;
}
impl InspectProviderCause for ProviderText {
    fn redact(&self, context: &Context) -> String {
        ProviderError::from_text(ProviderErrorKind::Internal, self.clone())
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

pub trait InitializeFixture: ProviderFactory + Sized {
    fn initialize_fixture(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, HostResult<InitializedProvider>> {
        let session = ProviderPlan::state_only().session(self.name(), binding, Some(config));
        Box::pin(async move { ProviderBoundary::initialize(self, &session).await })
    }
    fn initialize_normalizer_fixture(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, HostResult<ConfiguredNormalizer>> {
        let session = ProviderPlan::state_only().session(self.name(), binding, Some(config));
        Box::pin(async move { ProviderBoundary::initialize_normalizer(self, &session).await })
    }
}
impl<T: ProviderFactory> InitializeFixture for T {}

/// Explicitly opaque label for legacy unit probes; grants no Display implementation.
pub trait OpaqueFixtureLabel {
    fn to_string(&self) -> String {
        "<provider text>".into()
    }
}
impl OpaqueFixtureLabel for ProviderError {}
impl OpaqueFixtureLabel for ProviderDiagnostic {}
