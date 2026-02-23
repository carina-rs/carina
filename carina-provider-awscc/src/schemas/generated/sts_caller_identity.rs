//! sts_caller_identity schema definition for AWS Cloud Control
//!
//! Data source: returns the current AWS account ID, ARN, and user ID
//! via STS GetCallerIdentity.

use super::AwsccSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for sts_caller_identity
pub fn sts_caller_identity_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::STS::CallerIdentity",
        resource_type_name: "sts.caller_identity",
        has_tags: false,
        schema: ResourceSchema::new("awscc.sts.caller_identity")
            .with_description("Data source: STS GetCallerIdentity")
            .attribute(
                AttributeSchema::new("account_id", AttributeType::String)
                    .with_description("The AWS account ID of the caller. (read-only)")
                    .with_provider_name("AccountId"),
            )
            .attribute(
                AttributeSchema::new("arn", AttributeType::String)
                    .with_description("The ARN of the IAM principal making the call. (read-only)")
                    .with_provider_name("Arn"),
            )
            .attribute(
                AttributeSchema::new("user_id", AttributeType::String)
                    .with_description("The unique identifier of the calling entity. (read-only)")
                    .with_provider_name("UserId"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("sts.caller_identity", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
