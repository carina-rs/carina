//! nat_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.nat_gateway (Smithy: com.amazonaws.ec2)
pub fn ec2_nat_gateway_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::NatGateway",
        resource_type_name: "ec2.nat_gateway",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.nat_gateway")
            .with_description("Describes a NAT gateway.")
            .attribute(
                AttributeSchema::new("allocation_id", super::allocation_id())
                    .create_only()
                    .with_description(
                        "[Public NAT gateway only] The allocation ID of the Elastic IP address.",
                    )
                    .with_provider_name("AllocationId"),
            )
            .attribute(
                AttributeSchema::new(
                    "connectivity_type",
                    AttributeType::StringEnum {
                        name: "ConnectivityType".to_string(),
                        values: vec!["private".to_string(), "public".to_string()],
                        namespace: Some("aws.ec2.nat_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_description(
                    "Indicates whether the NAT gateway supports public or private connectivity.",
                )
                .with_provider_name("ConnectivityType"),
            )
            .attribute(
                AttributeSchema::new("nat_gateway_id", super::nat_gateway_id())
                    .with_description("The ID of the NAT gateway. (read-only)")
                    .with_provider_name("NatGatewayId"),
            )
            .attribute(
                AttributeSchema::new("subnet_id", super::subnet_id())
                    .required()
                    .create_only()
                    .with_description("The ID of the subnet in which the NAT gateway is placed.")
                    .with_provider_name("SubnetId"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags for the resource.")
                    .with_provider_name("Tags"),
            ),
    }
}

const VALID_CONNECTIVITY_TYPE: &[&str] = &["private", "public"];

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.nat_gateway",
        &[("connectivity_type", VALID_CONNECTIVITY_TYPE)],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
