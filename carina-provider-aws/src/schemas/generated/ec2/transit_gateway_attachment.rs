//! transit_gateway_attachment schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.transit_gateway_attachment (Smithy: com.amazonaws.ec2)
pub fn ec2_transit_gateway_attachment_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::TransitGatewayAttachment",
        resource_type_name: "ec2.transit_gateway_attachment",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.transit_gateway_attachment")
            .with_description("Describes a transit gateway VPC attachment.")
            .attribute(
                AttributeSchema::new(
                    "subnet_ids",
                    AttributeType::unordered_list(super::subnet_id()),
                )
                .required()
                .with_description("The IDs of one or more subnets.")
                .with_provider_name("SubnetIds"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("Any tags assigned to the transit gateway attachment.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new(
                    "transit_gateway_attachment_id",
                    super::transit_gateway_attachment_id(),
                )
                .with_description("The ID of the transit gateway attachment. (read-only)")
                .with_provider_name("TransitGatewayAttachmentId"),
            )
            .attribute(
                AttributeSchema::new("transit_gateway_id", super::transit_gateway_id())
                    .required()
                    .create_only()
                    .with_description("The ID of the transit gateway.")
                    .with_provider_name("TransitGatewayId"),
            )
            .attribute(
                AttributeSchema::new("vpc_id", super::vpc_id())
                    .required()
                    .create_only()
                    .with_description("The ID of the VPC.")
                    .with_provider_name("VpcId"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.transit_gateway_attachment", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
