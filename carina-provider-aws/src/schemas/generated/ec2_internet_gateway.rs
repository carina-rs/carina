//! internet_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2_internet_gateway (Smithy: com.amazonaws.ec2)
pub fn ec2_internet_gateway_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::InternetGateway",
        resource_type_name: "ec2_internet_gateway",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2_internet_gateway")
            .with_description("Describes an internet gateway.")
            .attribute(
                AttributeSchema::new("name", AttributeType::String)
                    .with_description("Resource name"),
            )
            .attribute(
                AttributeSchema::new("region", super::aws_region())
                    .with_description("The AWS region (inherited from provider if not specified)"),
            )
            .attribute(
                AttributeSchema::new("internet_gateway_id", super::internet_gateway_id())
                    .with_description("The ID of the internet gateway. (read-only)")
                    .with_provider_name("InternetGatewayId"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags for the resource.")
                    .with_provider_name("Tags"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_internet_gateway", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
