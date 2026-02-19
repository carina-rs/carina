//! eip schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::EIP
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

const VALID_DOMAIN: &[&str] = &["vpc", "standard"];

fn validate_domain(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(value, "Domain", "awscc.ec2_eip", VALID_DOMAIN).map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid Domain '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

/// Returns the schema config for ec2_eip (AWS::EC2::EIP)
pub fn ec2_eip_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::EIP",
        resource_type_name: "ec2_eip",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_eip")
        .with_description("Specifies an Elastic IP (EIP) address and can, optionally, associate it with an Amazon EC2 instance.  You can allocate an Elastic IP address from an address pool owned by AWS or from an address pool c...")
        .attribute(
            AttributeSchema::new("address", AttributeType::String)
                .create_only()
                .with_description("")
                .with_provider_name("Address"),
        )
        .attribute(
            AttributeSchema::new("allocation_id", super::aws_resource_id())
                .with_description(" (read-only)")
                .with_provider_name("AllocationId"),
        )
        .attribute(
            AttributeSchema::new("domain", AttributeType::Custom {
                name: "Domain".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_domain,
                namespace: Some("awscc.ec2_eip".to_string()),
                to_dsl: None,
            })
                .with_description("The network (``vpc``). If you define an Elastic IP address and associate it with a VPC that is defined in the same template, you must declare a depend...")
                .with_provider_name("Domain"),
        )
        .attribute(
            AttributeSchema::new("instance_id", super::aws_resource_id())
                .with_description("The ID of the instance.  Updates to the ``InstanceId`` property may require *some interruptions*. Updates on an EIP reassociates the address on its as...")
                .with_provider_name("InstanceId"),
        )
        .attribute(
            AttributeSchema::new("ipam_pool_id", super::ipam_pool_id())
                .create_only()
                .with_description("")
                .with_provider_name("IpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("network_border_group", AttributeType::String)
                .create_only()
                .with_description("A unique set of Availability Zones, Local Zones, or Wavelength Zones from which AWS advertises IP addresses. Use this parameter to limit the IP addres...")
                .with_provider_name("NetworkBorderGroup"),
        )
        .attribute(
            AttributeSchema::new("public_ip", types::ipv4_address())
                .with_description(" (read-only)")
                .with_provider_name("PublicIp"),
        )
        .attribute(
            AttributeSchema::new("public_ipv4_pool", AttributeType::String)
                .with_description("The ID of an address pool that you own. Use this parameter to let Amazon EC2 select an address from the address pool.  Updates to the ``PublicIpv4Pool...")
                .with_provider_name("PublicIpv4Pool"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the Elastic IP address.  Updates to the ``Tags`` property may require *some interruptions*. Updates on an EIP reassociates the ad...")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("transfer_address", AttributeType::String)
                .create_only()
                .with_description("The Elastic IP address you are accepting for transfer. You can only accept one transferred address. For more information on Elastic IP address transfe...")
                .with_provider_name("TransferAddress"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_eip", &[("domain", VALID_DOMAIN)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
