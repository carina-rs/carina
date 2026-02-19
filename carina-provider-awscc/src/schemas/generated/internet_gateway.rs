//! internet_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::InternetGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, ResourceSchema};

/// Returns the schema config for ec2_internet_gateway (AWS::EC2::InternetGateway)
pub fn ec2_internet_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::InternetGateway",
        resource_type_name: "ec2_internet_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_internet_gateway")
        .with_description("Allocates an internet gateway for use with a VPC. After creating the Internet gateway, you then attach it to a VPC.")
        .attribute(
            AttributeSchema::new("internet_gateway_id", super::internet_gateway_id())
                .with_description(" (read-only)")
                .with_provider_name("InternetGatewayId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags to assign to the internet gateway.")
                .with_provider_name("Tags"),
        )
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
