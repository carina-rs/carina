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
        has_tags: false,
        schema: ResourceSchema::new("awscc.ec2_vpc_gateway_attachment")
        .with_description("Resource Type definition for AWS::EC2::VPCGatewayAttachment")
        .attribute(
            AttributeSchema::new("attachment_type", AttributeType::String)
                .with_description("Used to identify if this resource is an Internet Gateway or Vpn Gateway Attachment  (read-only)")
                .with_provider_name("AttachmentType"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("The ID of the VPC.")
                .with_provider_name("VpcId"),
        )
        .attribute(
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description("The ID of the internet gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.")
                .with_provider_name("InternetGatewayId"),
        )
        .attribute(
            AttributeSchema::new("vpn_gateway_id", AttributeType::String)
                .with_description("The ID of the virtual private gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.")
                .with_provider_name("VpnGatewayId"),
        )
    }
}
