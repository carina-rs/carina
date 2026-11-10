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
    use carina_core::provider_text::Retryability;
    const MARKER: &str = "DependencyViolation";
    #[test]
    fn typed_delete_dependency_is_retryable_after_binding() {
        let error = ProviderError::api_error("dependent resource still exists")
            .with_retryability(Retryability::DependencyBlocked)
            .into_host_for_test();
        assert!(is_retryable_delete_error(&error));
        assert!(!error.is_transient());
    }

    #[test]
    fn timeout_is_not_retried_even_with_a_dependency_class() {
        let error = ProviderError::timeout("deadline elapsed")
            .with_retryability(Retryability::DependencyBlocked)
            .into_host_for_test();
        assert!(!is_retryable_delete_error(&error));
    }

    #[test]
    fn marker_text_in_message_cause_or_code_does_not_change_retryability() {
        for marker in [MARKER, "Rate exceeded", "ThrottlingException"] {
            let errors = [
                ProviderError::api_error(marker),
                ProviderError::api_error_caused_by("rejected", std::io::Error::other(marker)),
                ProviderError::api_error("rejected").with_code(marker),
            ];
            for error in errors {
                let error = error.into_host_for_test();
                assert_eq!(error.retryability(), Retryability::Unclassified);
                assert!(!is_retryable_delete_error(&error), "{error}");
                assert!(!error.is_delete_dependency());
                assert!(!error.is_transient());
            }
        }
        let error = ProviderError::api_error("temporary failure")
            .with_retryability(Retryability::Transient)
            .into_host_for_test();
        assert!(!is_retryable_delete_error(&error));
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
