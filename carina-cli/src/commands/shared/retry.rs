//! Retry helpers for `destroy`: delete-error classification and deletion polling.

use carina_core::provider::Provider;
use carina_core::resource::ResourceId;

/// Check if a delete error is retryable due to implicit dependency ordering.
///
/// Some AWS errors indicate that a resource cannot be deleted yet because
/// another resource still depends on it, even though there is no explicit
/// ResourceRef dependency. These errors are retryable: once the blocker is
/// deleted, the retry will succeed.
pub(crate) fn is_retryable_delete_error(e: &carina_core::provider::ProviderError) -> bool {
    if e.is_timeout {
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
        // CloudControl operation timeout — often caused by dependent resources
        // still being deleted (e.g., NAT Gateway blocking VPCGatewayAttachment)
        "Exceeded attempts to wait",
    ];
    retryable_patterns.iter().any(|p| msg.contains(p))
}

/// Result of waiting for a resource deletion to complete.
#[derive(Debug, PartialEq)]
pub(crate) enum WaitResult {
    /// Resource confirmed deleted (`state.exists == false`).
    Deleted,
    /// A `provider.read()` call returned an error.
    ReadError(String),
    /// The resource still existed after all retry attempts.
    TimedOut,
}

/// Poll `provider.read()` in a loop until the resource disappears or an error /
/// timeout occurs.
///
/// * `max_attempts` – how many times to poll (each preceded by `poll_interval`).
/// * `poll_interval` – sleep duration between polls.
pub(crate) async fn wait_for_deletion(
    provider: &dyn Provider,
    id: &ResourceId,
    identifier: &str,
    max_attempts: usize,
    poll_interval: std::time::Duration,
) -> WaitResult {
    for _ in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;
        match provider.read(id, Some(identifier)).await {
            Ok(state) if !state.exists => return WaitResult::Deleted,
            Ok(_) => {
                // Still exists, keep waiting
            }
            Err(e) => return WaitResult::ReadError(e.to_string()),
        }
    }
    WaitResult::TimedOut
}
