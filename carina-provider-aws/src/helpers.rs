//! Shared helper functions for the AWS provider.
//!
//! These reduce boilerplate across EC2 (and other) service implementations.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use aws_sdk_ec2::types::{ResourceType, Tag, TagSpecification};
use tokio::time::sleep;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, Value};

/// Extract a required `String` attribute from a resource.
///
/// Returns the string value or a `ProviderError` with `"{attr_name} is required"`.
pub fn require_string_attr(resource: &Resource, attr_name: &str) -> ProviderResult<String> {
    match resource.get_attr(attr_name) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(ProviderError::new(format!("{} is required", attr_name))
            .for_resource(resource.id.clone())),
    }
}

/// Build an EC2 `TagSpecification` from DSL tags for a given resource type.
///
/// Returns `None` if the resource has no `tags` attribute.
pub fn build_tag_specification(
    resource: &Resource,
    resource_type: ResourceType,
) -> Option<TagSpecification> {
    if let Some(Value::Map(tags)) = resource.get_attr("tags") {
        Some(build_tag_specification_from_map(tags, resource_type))
    } else {
        None
    }
}

/// Build an EC2 `TagSpecification` from a `HashMap` of tags.
fn build_tag_specification_from_map(
    tags: &HashMap<String, Value>,
    resource_type: ResourceType,
) -> TagSpecification {
    let mut tag_spec = TagSpecification::builder().resource_type(resource_type);
    for (key, val) in tags {
        if let Value::String(v) = val {
            tag_spec = tag_spec.tags(Tag::builder().key(key).value(v).build());
        }
    }
    tag_spec.build()
}

/// Represents the state returned by a poll function for `wait_for_ec2_state`.
pub enum PollState {
    /// The resource reached the desired state.
    Ready,
    /// The resource reached a terminal failure state.
    Failed,
    /// The resource no longer exists (useful for delete waits).
    Gone,
    /// The resource is still transitioning.
    Pending,
}

/// Retry an AWS SDK operation with exponential backoff on transient errors.
///
/// Only retries on known transient error patterns (throttling, service errors).
/// Does NOT retry on validation, permission, or resource-not-found errors.
///
/// - `operation_name`: Human-readable name for log messages.
/// - `max_attempts`: Maximum number of attempts (including the first).
/// - `initial_delay_secs`: Delay before the first retry (doubles each attempt, capped at 120s).
/// - `f`: A closure that returns a `Future` producing the SDK result.
pub async fn retry_aws_operation<F, Fut, T, E>(
    operation_name: &str,
    max_attempts: u32,
    initial_delay_secs: u64,
    f: F,
) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) if attempt < max_attempts && is_retryable_error(&e.to_string()) => {
                let delay = std::cmp::min(initial_delay_secs * 2u64.pow(attempt - 1), 120);
                eprintln!(
                    "  Retrying {} (attempt {}/{}): {}",
                    operation_name, attempt, max_attempts, e
                );
                sleep(Duration::from_secs(delay)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Check whether an error message indicates a transient AWS error worth retrying.
fn is_retryable_error(error_msg: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "ThrottlingException",
        "Throttling",
        "Rate exceeded",
        "RequestLimitExceeded",
        "ServiceUnavailable",
        "InternalError",
        "InternalServerError",
        "ServiceException",
    ];
    PATTERNS.iter().any(|p| error_msg.contains(p))
}

/// Generic wait/poll loop for EC2 resources.
///
/// Polls at 5-second intervals for up to `max_iterations` iterations.
///
/// - `poll_fn`: An async function that describes the resource and returns its `PollState`.
/// - `max_iterations`: Maximum number of poll iterations (each 5 seconds apart).
/// - `timeout_msg`: Error message if the loop times out.
/// - `failure_msg`: Error message if the resource reaches a failed state.
pub async fn wait_for_ec2_state<F, Fut>(
    id: &ResourceId,
    poll_fn: F,
    max_iterations: u32,
    timeout_msg: &str,
    failure_msg: &str,
) -> ProviderResult<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProviderResult<PollState>>,
{
    for _ in 0..max_iterations {
        match poll_fn().await? {
            PollState::Ready => return Ok(()),
            PollState::Gone => return Ok(()),
            PollState::Failed => {
                return Err(ProviderError::new(failure_msg).for_resource(id.clone()));
            }
            PollState::Pending => {}
        }
        sleep(Duration::from_secs(5)).await;
    }

    Err(ProviderError::new(timeout_msg).for_resource(id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable_error_throttling() {
        assert!(is_retryable_error("ThrottlingException: Rate exceeded"));
        assert!(is_retryable_error("Throttling: request limit"));
        assert!(is_retryable_error("Rate exceeded for API call"));
        assert!(is_retryable_error("RequestLimitExceeded"));
    }

    #[test]
    fn test_is_retryable_error_service_errors() {
        assert!(is_retryable_error("ServiceUnavailable: try again"));
        assert!(is_retryable_error("InternalError occurred"));
        assert!(is_retryable_error("InternalServerError"));
        assert!(is_retryable_error("ServiceException: transient"));
    }

    #[test]
    fn test_is_retryable_error_non_retryable() {
        assert!(!is_retryable_error("ValidationError: invalid parameter"));
        assert!(!is_retryable_error("AccessDeniedException: not authorized"));
        assert!(!is_retryable_error("ResourceNotFoundException: not found"));
        assert!(!is_retryable_error("InvalidParameterValue"));
    }

    #[tokio::test]
    async fn test_retry_aws_operation_succeeds_first_try() {
        let result: Result<&str, String> =
            retry_aws_operation("test op", 3, 1, || async { Ok("success") }).await;
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_retry_aws_operation_non_retryable_fails_immediately() {
        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = attempt_count.clone();
        let result: Result<&str, String> = retry_aws_operation("test op", 3, 1, || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err("ValidationError: bad input".to_string())
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "should not retry non-retryable errors"
        );
    }
}
