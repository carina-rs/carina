//! vpc_gateway_attachment schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPCGatewayAttachment
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2_vpc_gateway_attachment (AWS::EC2::VPCGatewayAttachment)
pub fn ec2_vpc_gateway_attachment_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::VPCGatewayAttachment",
        resource_type_name: "ec2.vpc_gateway_attachment",
        has_tags: false,
        schema: ResourceSchema::new("awscc.ec2.vpc_gateway_attachment")
        .with_description("Resource Type definition for AWS::EC2::VPCGatewayAttachment")
        .attribute(
            AttributeSchema::new("attachment_type", AttributeType::String)
                .with_description("Used to identify if this resource is an Internet Gateway or Vpn Gateway Attachment  (read-only)")
                .with_provider_name("AttachmentType"),
        )
        .attribute(
            AttributeSchema::new("internet_gateway_id", super::internet_gateway_id())
                .with_description("The ID of the internet gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.")
                .with_provider_name("InternetGatewayId"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .required()
                .create_only()
                .with_description("The ID of the VPC.")
                .with_provider_name("VpcId"),
        )
        .attribute(
            AttributeSchema::new("vpn_gateway_id", super::vpn_gateway_id())
                .with_description("The ID of the virtual private gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.")
                .with_provider_name("VpnGatewayId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.vpc_gateway_attachment", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
