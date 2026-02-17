//! vpn_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPNGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2_vpn_gateway (AWS::EC2::VPNGateway)
pub fn ec2_vpn_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::VPNGateway",
        resource_type_name: "ec2_vpn_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_vpn_gateway")
        .with_description("Specifies a virtual private gateway. A virtual private gateway is the endpoint on the VPC side of your VPN connection. You can create a virtual private gateway before creating the VPC itself.  For mor...")
        .attribute(
            AttributeSchema::new("amazon_side_asn", AttributeType::Int)
                .create_only()
                .with_description("The private Autonomous System Number (ASN) for the Amazon side of a BGP session.")
                .with_provider_name("AmazonSideAsn"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the virtual private gateway.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("type", AttributeType::String)
                .required()
                .create_only()
                .with_description("The type of VPN connection the virtual private gateway supports.")
                .with_provider_name("Type"),
        )
        .attribute(
            AttributeSchema::new("vpn_gateway_id", super::vpn_gateway_id())
                .with_description(" (read-only)")
                .with_provider_name("VPNGatewayId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_vpn_gateway", &[])
}
