//! caller_identity schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.sts
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for sts.caller_identity (Smithy: com.amazonaws.sts)
pub fn sts_caller_identity_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::STS::CallerIdentity",
        resource_type_name: "sts.caller_identity",
        has_tags: false,
        schema: ResourceSchema::new("aws.sts.caller_identity")
        .as_data_source()
        .attribute(
            AttributeSchema::new("account_id", super::aws_account_id())
                .with_description("The Amazon Web Services account ID number of the account that owns or contains the calling entity. (read-only)")
                .with_provider_name("AccountId"),
        )
        .attribute(
            AttributeSchema::new("arn", super::arn())
                .with_description("The Amazon Web Services ARN associated with the calling entity. (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("user_id", AttributeType::String)
                .with_description("The unique identifier of the calling entity. The exact value depends on the type of entity that is making the call. The values returned are those list... (read-only)")
                .with_provider_name("UserId"),
        )
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
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
