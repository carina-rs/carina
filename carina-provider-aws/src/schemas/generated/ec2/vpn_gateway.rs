//! vpn_gateway schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.vpn_gateway (Smithy: com.amazonaws.ec2)
pub fn ec2_vpn_gateway_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::VPNGateway",
        resource_type_name: "ec2.vpn_gateway",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.vpn_gateway")
            .with_description("Describes a virtual private gateway.")
            .attribute(
                AttributeSchema::new("amazon_side_asn", AttributeType::Int)
                    .create_only()
                    .with_description(
                        "The private Autonomous System Number (ASN) for the Amazon side of a BGP session.",
                    )
                    .with_provider_name("AmazonSideAsn"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("Any tags assigned to the virtual private gateway.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new(
                    "type",
                    AttributeType::StringEnum {
                        name: "Type".to_string(),
                        values: vec!["ipsec.1".to_string()],
                        namespace: Some("aws.ec2.vpn_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .required()
                .create_only()
                .with_description(
                    "The type of VPN connection the virtual private gateway supports.",
                )
                .with_provider_name("Type"),
            )
            .attribute(
                AttributeSchema::new("vpn_gateway_id", super::vpn_gateway_id())
                    .with_description("The ID of the virtual private gateway. (read-only)")
                    .with_provider_name("VpnGatewayId"),
            ),
    }
}

const VALID_TYPE: &[&str] = &["ipsec.1"];

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.vpn_gateway", &[("type", VALID_TYPE)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
