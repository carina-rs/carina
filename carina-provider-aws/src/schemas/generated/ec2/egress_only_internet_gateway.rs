//! egress_only_internet_gateway schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, ResourceSchema};

/// Returns the schema config for ec2.egress_only_internet_gateway (Smithy: com.amazonaws.ec2)
pub fn ec2_egress_only_internet_gateway_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::EgressOnlyInternetGateway",
        resource_type_name: "ec2.egress_only_internet_gateway",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.egress_only_internet_gateway")
            .with_description("Describes an egress-only internet gateway.")
            .attribute(
                AttributeSchema::new(
                    "egress_only_internet_gateway_id",
                    super::egress_only_internet_gateway_id(),
                )
                .with_description("The ID of the egress-only internet gateway. (read-only)")
                .with_provider_name("EgressOnlyInternetGatewayId"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("Any tags assigned to the egress-only internet gateway.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new("vpc_id", super::vpc_id())
                    .required()
                    .create_only()
                    .with_description(
                        "The ID of the VPC for which to create the egress-only internet gateway.",
                    )
                    .with_provider_name("VpcId"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.egress_only_internet_gateway", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
