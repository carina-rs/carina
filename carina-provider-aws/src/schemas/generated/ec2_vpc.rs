//! vpc schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

#[allow(dead_code)]
const VALID_INSTANCE_TENANCY: &[&str] = &["dedicated", "default", "host"];

#[allow(dead_code)]
fn validate_instance_tenancy(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "InstanceTenancy",
        "aws.ec2_vpc",
        VALID_INSTANCE_TENANCY,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid InstanceTenancy '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

fn validate_ipv4_netmask_length_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 0 || *n > 32 {
            Err(format!("Value {} is out of range 0..=32", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for ec2_vpc (Smithy: com.amazonaws.ec2)
pub fn ec2_vpc_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::VPC",
        resource_type_name: "ec2_vpc",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2_vpc")
        .with_description("Describes a VPC.")
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .with_description("Resource name"),
        )
        .attribute(
            AttributeSchema::new("region", super::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::ipv4_cidr())
                .create_only()
                .with_description("The IPv4 network range for the VPC, in CIDR notation. For example, 10.0.0.0/16. We modify the specified CIDR block to its canonical form; for example,...")
                .with_provider_name("CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("enable_dns_hostnames", AttributeType::Bool)
                .with_description("Indicates whether the instances launched in the VPC get DNS hostnames. If enabled, instances in the VPC get DNS hostnames; otherwise, they do not. You...")
                .with_provider_name("EnableDnsHostnames"),
        )
        .attribute(
            AttributeSchema::new("enable_dns_support", AttributeType::Bool)
                .with_description("Indicates whether the DNS resolution is supported for the VPC. If enabled, queries to the Amazon provided DNS server at the 169.254.169.253 IP address...")
                .with_provider_name("EnableDnsSupport"),
        )
        .attribute(
            AttributeSchema::new("instance_tenancy", AttributeType::Custom {
                name: "InstanceTenancy".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_instance_tenancy,
                namespace: Some("aws.ec2_vpc".to_string()),
                to_dsl: None,
            })
                .create_only()
                .with_description("The tenancy options for instances launched into the VPC. For default, instances are launched with shared tenancy by default. You can launch instances ...")
                .with_provider_name("InstanceTenancy"),
        )
        .attribute(
            AttributeSchema::new("ipv4_ipam_pool_id", super::ipam_pool_id())
                .create_only()
                .with_description("The ID of an IPv4 IPAM pool you want to use for allocating this VPC's CIDR. For more information, see What is IPAM? in the Amazon VPC IPAM User Guide.")
                .with_provider_name("Ipv4IpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("ipv4_netmask_length", AttributeType::Custom {
                name: "Int(0..=32)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_ipv4_netmask_length_range,
                namespace: None,
                to_dsl: None,
            })
                .create_only()
                .with_description("The netmask length of the IPv4 CIDR you want to allocate to this VPC from an Amazon VPC IP Address Manager (IPAM) pool. For more information about IPA...")
                .with_provider_name("Ipv4NetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .with_description("The ID of the VPC. (read-only)")
                .with_provider_name("VpcId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the resource.")
                .with_provider_name("Tags"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_vpc", &[("instance_tenancy", VALID_INSTANCE_TENANCY)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
