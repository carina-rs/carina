//! vpn_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPNGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

const VALID_TYPE: &[&str] = &["ipsec.1"];

fn validate_amazon_side_asn_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 1 || *n > 4294967294 {
            Err(format!("Value {} is out of range 1..=4294967294", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for ec2_vpn_gateway (AWS::EC2::VPNGateway)
pub fn ec2_vpn_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::VPNGateway",
        resource_type_name: "ec2.vpn_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.vpn_gateway")
        .with_description("Specifies a virtual private gateway. A virtual private gateway is the endpoint on the VPC side of your VPN connection. You can create a virtual private gateway before creating the VPC itself.  For more information, see [](https://docs.aws.amazon.com/vpn/latest/s2svpn/VPC_VPN.html) in the *User Guide*.")
        .attribute(
            AttributeSchema::new("amazon_side_asn", AttributeType::Custom {
                name: "Int(1..=4294967294)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_amazon_side_asn_range,
                namespace: None,
                to_dsl: None,
            })
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
            AttributeSchema::new("type", AttributeType::StringEnum {
                name: "Type".to_string(),
                values: vec!["ipsec.1".to_string()],
                namespace: Some("awscc.ec2.vpn_gateway".to_string()),
                to_dsl: None,
            })
                .required()
                .create_only()
                .with_description("The type of VPN connection the virtual private gateway supports.")
                .with_provider_name("Type"),
        )
        .attribute(
            AttributeSchema::new("vpn_gateway_id", super::vpn_gateway_id())
                .read_only()
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
    ("ec2.vpn_gateway", &[("type", VALID_TYPE)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
