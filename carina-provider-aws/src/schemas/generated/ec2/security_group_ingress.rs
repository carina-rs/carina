//! security_group_ingress schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

const VALID_IP_PROTOCOL: &[&str] = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];

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

/// Returns the schema config for ec2.security_group_ingress (Smithy: com.amazonaws.ec2)
pub fn ec2_security_group_ingress_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroupIngress",
        resource_type_name: "ec2.security_group_ingress",
        has_tags: false,
        schema: ResourceSchema::new("aws.ec2.security_group_ingress")
        .with_description("Describes a security group rule.")
        .attribute(
            AttributeSchema::new("cidr_ip", types::ipv4_cidr())
                .create_only()
                .with_description("The IPv4 address range, in CIDR format. Amazon Web Services canonicalizes IPv4 and IPv6 CIDRs. For example, if you specify 100.68.0.18/18 for the CIDR...")
                .with_provider_name("CidrIp"),
        )
        .attribute(
            AttributeSchema::new("cidr_ipv6", types::ipv6_cidr())
                .create_only()
                .with_description("The IPv6 CIDR range.")
                .with_provider_name("CidrIpv6"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .create_only()
                .with_description("The security group rule description.")
                .with_provider_name("Description"),
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
                .with_description("If the protocol is TCP or UDP, this is the start of the port range. If the protocol is ICMP, this is the ICMP type or -1 (all ICMP types). To specify ...")
                .with_provider_name("FromPort"),
        )
        .attribute(
            AttributeSchema::new("group_id", super::security_group_id())
                .create_only()
                .with_description("The ID of the security group.")
                .with_provider_name("GroupId"),
        )
        .attribute(
            AttributeSchema::new("group_name", AttributeType::String)
                .create_only()
                .with_description("[Default VPC] The name of the security group. For security groups for a default VPC you can specify either the ID or the name of the security group. F...")
                .with_provider_name("GroupName"),
        )
        .attribute(
            AttributeSchema::new("ip_protocol", AttributeType::StringEnum {
                name: "IpProtocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string(), "icmpv6".to_string(), "-1".to_string(), "all".to_string()],
                namespace: Some("aws.ec2.security_group_ingress".to_string()),
                to_dsl: Some(|s: &str| match s { "-1" => "all".to_string(), _ => s.replace('-', "_") }),
            })
                .required()
                .create_only()
                .with_description("The IP protocol name (tcp, udp, icmp) or number (see Protocol Numbers). To specify all protocols, use -1. To specify icmpv6, use IP permissions instea...")
                .with_provider_name("IpProtocol"),
        )
        .attribute(
            AttributeSchema::new("source_prefix_list_id", super::prefix_list_id())
                .create_only()
                .with_description("The ID of the source prefix list.")
                .with_provider_name("SourcePrefixListId"),
        )
        .attribute(
            AttributeSchema::new("source_security_group_name", AttributeType::String)
                .create_only()
                .with_description("[Default VPC] The name of the source security group. The rule grants full ICMP, UDP, and TCP access. To create a rule with a specific protocol and por...")
                .with_provider_name("SourceSecurityGroupName"),
        )
        .attribute(
            AttributeSchema::new("source_security_group_owner_id", super::aws_account_id())
                .create_only()
                .with_description("The Amazon Web Services account ID for the source security group, if the source security group is in a different account. The rule grants full ICMP, U...")
                .with_provider_name("SourceSecurityGroupOwnerId"),
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
                .with_description("If the protocol is TCP or UDP, this is the end of the port range. If the protocol is ICMP, this is the ICMP code or -1 (all ICMP codes). If the start ...")
                .with_provider_name("ToPort"),
        )
        .attribute(
            AttributeSchema::new("source_security_group_id", super::security_group_id())
                .create_only()
                .with_description("The ID of the source security group.")
                .with_provider_name("SourceSecurityGroupId"),
        )
        .attribute(
            AttributeSchema::new("security_group_rule_id", super::security_group_rule_id())
                .with_description("The ID of the security group rule. (read-only)")
                .with_provider_name("SecurityGroupRuleId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.security_group_ingress",
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
