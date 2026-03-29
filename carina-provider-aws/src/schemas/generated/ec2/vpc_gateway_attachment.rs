//! vpc_gateway_attachment schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use carina_core::schema::{AttributeSchema, ResourceSchema, validators};

/// Returns the schema config for ec2.vpc_gateway_attachment (Smithy: com.amazonaws.ec2)
pub fn ec2_vpc_gateway_attachment_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::VPCGatewayAttachment",
        resource_type_name: "ec2.vpc_gateway_attachment",
        has_tags: false,
        schema: ResourceSchema::new("aws.ec2.vpc_gateway_attachment")
            .with_description("Attaches an internet gateway or virtual private gateway to a VPC.")
            .attribute(
                AttributeSchema::new("internet_gateway_id", super::internet_gateway_id())
                    .with_description("The ID of the internet gateway. Specify either this or vpn_gateway_id, but not both.")
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
                    .with_description("The ID of the virtual private gateway. Specify either this or internet_gateway_id, but not both.")
                    .with_provider_name("VpnGatewayId"),
            )
            .with_validator(|attrs| {
                let mut errors = Vec::new();
                if let Err(mut e) = validators::validate_exclusive_required(attrs, &["internet_gateway_id", "vpn_gateway_id"]) {
                    errors.append(&mut e);
                }
                if errors.is_empty() { Ok(()) } else { Err(errors) }
            }),
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
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
