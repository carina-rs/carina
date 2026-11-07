//! Retry helpers for `destroy` delete-error classification.

/// Check if a delete error is retryable due to implicit dependency ordering.
///
/// Some AWS errors indicate that a resource cannot be deleted yet because
/// another resource still depends on it, even though there is no explicit
/// ResourceRef dependency. These errors are retryable: once the blocker is
/// deleted, the retry will succeed.
pub(crate) fn is_retryable_delete_error(e: &carina_core::provider::HostError) -> bool {
    if matches!(e, carina_core::provider::HostError::Timeout(_)) {
        return false;
    }
    e.is_delete_dependency()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_text_fixtures::BindProviderText;
    use carina_core::provider::ProviderError;
    const MARKER: &str = "DependencyViolation";
    #[test]
    fn review_round6_provider_markers_come_from_message_cause_and_code() {
        let errors = [
            ProviderError::api_error(MARKER),
            ProviderError::api_error_caused_by("rejected", std::io::Error::other(MARKER)),
            ProviderError::api_error("rejected").with_code(MARKER),
        ];
        for error in errors {
            let error = error.into_host_for_test();
            assert!(is_retryable_delete_error(&error), "{error}");
        }
    }
    #[test]
    fn review_round6_later_context_is_not_a_provider_marker() {
        let id = carina_core::resource::ResourceId::with_provider_identity(
            "test", "Resource", MARKER, None,
        );
        let errors = [
            ProviderError::api_error("rejected").for_resource(id),
            ProviderError::api_error("rejected").for_provider(MARKER),
            ProviderError::api_error("rejected").with_operation(MARKER),
            ProviderError::api_error("rejected").with_request_id(MARKER),
        ];
        for error in errors {
            let error = error.into_host_for_test();
            assert!(
                !is_retryable_delete_error(&error),
                "context changed retry classification: {error}"
            );
        }
    }
}
