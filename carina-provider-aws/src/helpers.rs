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
pub fn build_tag_specification_from_map(
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

/// Generic wait/poll loop for EC2 resources.
///
/// Polls at 5-second intervals for up to 60 iterations (5 minutes).
///
/// - `poll_fn`: An async function that describes the resource and returns its `PollState`.
/// - `timeout_msg`: Error message if the loop times out.
/// - `failure_msg`: Error message if the resource reaches a failed state.
pub async fn wait_for_ec2_state<F, Fut>(
    id: &ResourceId,
    poll_fn: F,
    timeout_msg: &str,
    failure_msg: &str,
) -> ProviderResult<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProviderResult<PollState>>,
{
    for _ in 0..60 {
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
