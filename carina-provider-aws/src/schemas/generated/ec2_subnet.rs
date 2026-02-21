//! subnet schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField, types};

#[allow(dead_code)]
const VALID_HOSTNAME_TYPE: &[&str] = &["ip-name", "resource-name"];

#[allow(dead_code)]
fn validate_hostname_type(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(value, "HostnameType", "aws.ec2.subnet", VALID_HOSTNAME_TYPE).map_err(
        |reason| {
            if let Value::String(s) = value {
                format!("Invalid HostnameType '{}': {}", s, reason)
            } else {
                reason
            }
        },
    )
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

fn validate_ipv6_netmask_length_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 0 || *n > 128 {
            Err(format!("Value {} is out of range 0..=128", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for ec2.subnet (Smithy: com.amazonaws.ec2)
pub fn ec2_subnet_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::Subnet",
        resource_type_name: "ec2.subnet",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.subnet")
        .with_description("Describes a subnet.")
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .with_description("Resource name"),
        )
        .attribute(
            AttributeSchema::new("region", super::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("assign_ipv6_address_on_creation", AttributeType::Bool)
                .with_description("Indicates whether a network interface created in this subnet (including a network interface created by RunInstances) receives an IPv6 address.")
                .with_provider_name("AssignIpv6AddressOnCreation"),
        )
        .attribute(
            AttributeSchema::new("availability_zone", super::availability_zone())
                .create_only()
                .with_description("The Availability Zone or Local Zone for the subnet. Default: Amazon Web Services selects one for you. If you create more than one subnet in your VPC, ...")
                .with_provider_name("AvailabilityZone"),
        )
        .attribute(
            AttributeSchema::new("availability_zone_id", AttributeType::String)
                .create_only()
                .with_description("The AZ ID or the Local Zone ID of the subnet.")
                .with_provider_name("AvailabilityZoneId"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::ipv4_cidr())
                .create_only()
                .with_description("The IPv4 network range for the subnet, in CIDR notation. For example, 10.0.0.0/24. We modify the specified CIDR block to its canonical form; for examp...")
                .with_provider_name("CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("enable_dns64", AttributeType::Bool)
                .with_description("Indicates whether DNS queries made to the Amazon-provided DNS Resolver in this subnet should return synthetic IPv6 addresses for IPv4-only destination...")
                .with_provider_name("EnableDns64"),
        )
        .attribute(
            AttributeSchema::new("enable_lni_at_device_index", AttributeType::Int)
                .with_description("Indicates the device position for local network interfaces in this subnet. For example, 1 indicates local network interfaces in this subnet are the se...")
                .with_provider_name("EnableLniAtDeviceIndex"),
        )
        .attribute(
            AttributeSchema::new("ipv4_ipam_pool_id", super::ipam_pool_id())
                .create_only()
                .with_description("An IPv4 IPAM pool ID for the subnet.")
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
                .with_description("An IPv4 netmask length for the subnet.")
                .with_provider_name("Ipv4NetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_block", types::ipv6_cidr())
                .create_only()
                .with_description("The IPv6 network range for the subnet, in CIDR notation. This parameter is required for an IPv6 only subnet.")
                .with_provider_name("Ipv6CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("ipv6_ipam_pool_id", super::ipam_pool_id())
                .create_only()
                .with_description("An IPv6 IPAM pool ID for the subnet.")
                .with_provider_name("Ipv6IpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("ipv6_native", AttributeType::Bool)
                .create_only()
                .with_description("Indicates whether to create an IPv6 only subnet.")
                .with_provider_name("Ipv6Native"),
        )
        .attribute(
            AttributeSchema::new("ipv6_netmask_length", AttributeType::Custom {
                name: "Int(0..=128)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_ipv6_netmask_length_range,
                namespace: None,
                to_dsl: None,
            })
                .create_only()
                .with_description("An IPv6 netmask length for the subnet.")
                .with_provider_name("Ipv6NetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("map_public_ip_on_launch", AttributeType::Bool)
                .with_description("Indicates whether instances launched in this subnet receive a public IPv4 address. Amazon Web Services charges for all public IPv4 addresses, includin...")
                .with_provider_name("MapPublicIpOnLaunch"),
        )
        .attribute(
            AttributeSchema::new("outpost_arn", super::arn())
                .create_only()
                .with_description("The Amazon Resource Name (ARN) of the Outpost. If you specify an Outpost ARN, you must also specify the Availability Zone of the Outpost subnet.")
                .with_provider_name("OutpostArn"),
        )
        .attribute(
            AttributeSchema::new("private_dns_name_options_on_launch", AttributeType::Struct {
                    name: "PrivateDnsNameOptionsOnLaunch".to_string(),
                    fields: vec![
                    StructField::new("enable_resource_name_dns_aaaa_record", AttributeType::Bool).with_description("Indicates whether to respond to DNS queries for instance hostname with DNS AAAA records.").with_provider_name("EnableResourceNameDnsAAAARecord"),
                    StructField::new("enable_resource_name_dns_a_record", AttributeType::Bool).with_description("Indicates whether to respond to DNS queries for instance hostnames with DNS A records.").with_provider_name("EnableResourceNameDnsARecord"),
                    StructField::new("hostname_type", AttributeType::Custom {
                name: "HostnameType".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_hostname_type,
                namespace: Some("aws.ec2.subnet".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            }).with_description("The type of hostname for EC2 instances. For IPv4 only subnets, an instance DNS name must be based on the instance IPv4 address. For IPv6 only subnets,...").with_provider_name("HostnameType")
                    ],
                })
                .with_description("The type of hostnames to assign to instances in the subnet at launch. An instance hostname is based on the IPv4 address or ID of the instance.")
                .with_provider_name("PrivateDnsNameOptionsOnLaunch"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .required()
                .create_only()
                .with_description("The ID of the VPC.")
                .with_provider_name("VpcId"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", super::subnet_id())
                .with_description("The ID of the subnet. (read-only)")
                .with_provider_name("SubnetId"),
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
    ("ec2.subnet", &[("hostname_type", VALID_HOSTNAME_TYPE)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
