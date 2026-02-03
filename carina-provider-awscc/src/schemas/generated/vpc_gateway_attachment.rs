//! ec2_vpc_gateway_attachment schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPCGatewayAttachment
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_vpc_gateway_attachment (AWS::EC2::VPCGatewayAttachment)
pub fn ec2_vpc_gateway_attachment_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_vpc_gateway_attachment")
        .with_description("Attaches an internet gateway, or a virtual private gateway to a VPC, enabling connectivity between the internet and the VPC.")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("The ID of the VPC."),
        )
        .attribute(
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description("The ID of the internet gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both."),
        )
        .attribute(
            AttributeSchema::new("vpn_gateway_id", AttributeType::String)
                .with_description("The ID of the virtual private gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both."),
        )
}
