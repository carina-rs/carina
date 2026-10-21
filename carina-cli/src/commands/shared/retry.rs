//! Retry helpers for `destroy` delete-error classification.

/// Check if a delete error is retryable due to implicit dependency ordering.
///
/// Some AWS errors indicate that a resource cannot be deleted yet because
/// another resource still depends on it, even though there is no explicit
/// ResourceRef dependency. These errors are retryable: once the blocker is
/// deleted, the retry will succeed.
pub(crate) fn is_retryable_delete_error(e: &carina_core::provider::ProviderError) -> bool {
    if matches!(e, carina_core::provider::ProviderError::Timeout(_)) {
        return false;
    }
    let msg = e.to_string();
    let retryable_patterns = [
        "DependencyViolation",
        "has dependent object",
        "has a dependent object",
        "resource has dependencies",
        "mapped public address",
        "Failed to detach",
    ];
    retryable_patterns.iter().any(|p| msg.contains(p))
}
