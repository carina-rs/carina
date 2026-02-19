//! security_group_egress schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SecurityGroupEgress
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

const VALID_IP_PROTOCOL: &[&str] = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];

fn validate_ip_protocol(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "IpProtocol",
        "awscc.ec2_security_group_egress",
        VALID_IP_PROTOCOL,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid IpProtocol '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

fn validate_from_port_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < -1 || *n > 65535 {
            Err(format!("Value {} is out of range -1..=65535", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

fn validate_to_port_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < -1 || *n > 65535 {
            Err(format!("Value {} is out of range -1..=65535", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for ec2_security_group_egress (AWS::EC2::SecurityGroupEgress)
pub fn ec2_security_group_egress_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroupEgress",
        resource_type_name: "ec2_security_group_egress",
        has_tags: false,
        schema: ResourceSchema::new("awscc.ec2_security_group_egress")
        .with_description("Adds the specified outbound (egress) rule to a security group.  An outbound rule permits instances to send traffic to the specified IPv4 or IPv6 address range, the IP addresses that are specified by a...")
        .attribute(
            AttributeSchema::new("cidr_ip", types::ipv4_cidr())
                .create_only()
                .with_description("The IPv4 address range, in CIDR format. You must specify exactly one of the following: ``CidrIp``, ``CidrIpv6``, ``DestinationPrefixListId``, or ``Des...")
                .with_provider_name("CidrIp"),
        )
        .attribute(
            AttributeSchema::new("cidr_ipv6", types::ipv6_cidr())
                .create_only()
                .with_description("The IPv6 address range, in CIDR format. You must specify exactly one of the following: ``CidrIp``, ``CidrIpv6``, ``DestinationPrefixListId``, or ``Des...")
                .with_provider_name("CidrIpv6"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_description("The description of an egress (outbound) security group rule. Constraints: Up to 255 characters in length. Allowed characters are a-z, A-Z, 0-9, spaces...")
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("destination_prefix_list_id", super::aws_resource_id())
                .create_only()
                .with_description("The prefix list IDs for an AWS service. This is the AWS service to access through a VPC endpoint from instances associated with the security group. Yo...")
                .with_provider_name("DestinationPrefixListId"),
        )
        .attribute(
            AttributeSchema::new("destination_security_group_id", super::security_group_id())
                .create_only()
                .with_description("The ID of the security group. You must specify exactly one of the following: ``CidrIp``, ``CidrIpv6``, ``DestinationPrefixListId``, or ``DestinationSe...")
                .with_provider_name("DestinationSecurityGroupId"),
        )
        .attribute(
            AttributeSchema::new("from_port", AttributeType::Custom {
                name: "Int(-1..=65535)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_from_port_range,
                namespace: None,
                to_dsl: None,
            })
                .create_only()
                .with_description("If the protocol is TCP or UDP, this is the start of the port range. If the protocol is ICMP or ICMPv6, this is the ICMP type or -1 (all ICMP types).")
                .with_provider_name("FromPort"),
        )
        .attribute(
            AttributeSchema::new("group_id", super::security_group_id())
                .required()
                .create_only()
                .with_description("The ID of the security group. You must specify either the security group ID or the security group name in the request. For security groups in a nondef...")
                .with_provider_name("GroupId"),
        )
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("ip_protocol", AttributeType::Custom {
                name: "IpProtocol".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_ip_protocol,
                namespace: Some("awscc.ec2_security_group_egress".to_string()),
                to_dsl: Some(|s: &str| match s { "-1" => "all".to_string(), _ => s.replace('-', "_") }),
            })
                .required()
                .create_only()
                .with_description("The IP protocol name (``tcp``, ``udp``, ``icmp``, ``icmpv6``) or number (see [Protocol Numbers](https://docs.aws.amazon.com/http://www.iana.org/assign...")
                .with_provider_name("IpProtocol"),
        )
        .attribute(
            AttributeSchema::new("to_port", AttributeType::Custom {
                name: "Int(-1..=65535)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_to_port_range,
                namespace: None,
                to_dsl: None,
            })
                .create_only()
                .with_description("If the protocol is TCP or UDP, this is the end of the port range. If the protocol is ICMP or ICMPv6, this is the ICMP code or -1 (all ICMP codes). If ...")
                .with_provider_name("ToPort"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2_security_group_egress",
        &[("ip_protocol", VALID_IP_PROTOCOL)],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    match (attr_name, value) {
        ("ip_protocol", "all") => Some("-1"),
        _ => None,
    }
}
