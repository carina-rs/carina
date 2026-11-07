use carina_core::provider_text::ProviderCause;

pub fn matches(cause: ProviderCause<'_>) -> bool {
    cause.matches_for_test::<std::io::Error>(|_| true)
}
