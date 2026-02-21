//! security_group_egress schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

#[allow(dead_code)]
const VALID_IP_PROTOCOL: &[&str] = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];

#[allow(dead_code)]
fn validate_ip_protocol(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "IpProtocol",
        "aws.ec2_security_group_egress",
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

/// Returns the schema config for ec2_security_group_egress (Smithy: com.amazonaws.ec2)
pub fn ec2_security_group_egress_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroupEgress",
        resource_type_name: "ec2_security_group_egress",
        has_tags: false,
        schema: ResourceSchema::new("aws.ec2_security_group_egress")
            .with_description("Describes a security group rule.")
            .attribute(
                AttributeSchema::new("name", AttributeType::String)
                    .with_description("Resource name"),
            )
            .attribute(
                AttributeSchema::new("region", super::aws_region())
                    .with_description("The AWS region (inherited from provider if not specified)"),
            )
            .attribute(
                AttributeSchema::new("cidr_ip", types::ipv4_cidr())
                    .create_only()
                    .with_description("Not supported. Use IP permissions instead.")
                    .with_provider_name("CidrIp"),
            )
            .attribute(
                AttributeSchema::new(
                    "from_port",
                    AttributeType::Custom {
                        name: "Int(-1..=65535)".to_string(),
                        base: Box::new(AttributeType::Int),
                        validate: validate_from_port_range,
                        namespace: None,
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_description("Not supported. Use IP permissions instead.")
                .with_provider_name("FromPort"),
            )
            .attribute(
                AttributeSchema::new("group_id", super::security_group_id())
                    .required()
                    .create_only()
                    .with_description("The ID of the security group.")
                    .with_provider_name("GroupId"),
            )
            .attribute(
                AttributeSchema::new(
                    "ip_protocol",
                    AttributeType::Custom {
                        name: "IpProtocol".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_ip_protocol,
                        namespace: Some("aws.ec2_security_group_egress".to_string()),
                        to_dsl: Some(|s: &str| match s {
                            "-1" => "all".to_string(),
                            _ => s.replace('-', "_"),
                        }),
                    },
                )
                .required()
                .create_only()
                .with_description("Not supported. Use IP permissions instead.")
                .with_provider_name("IpProtocol"),
            )
            .attribute(
                AttributeSchema::new("source_security_group_name", AttributeType::String)
                    .create_only()
                    .with_description("Not supported. Use IP permissions instead.")
                    .with_provider_name("SourceSecurityGroupName"),
            )
            .attribute(
                AttributeSchema::new("source_security_group_owner_id", AttributeType::String)
                    .create_only()
                    .with_description("Not supported. Use IP permissions instead.")
                    .with_provider_name("SourceSecurityGroupOwnerId"),
            )
            .attribute(
                AttributeSchema::new(
                    "to_port",
                    AttributeType::Custom {
                        name: "Int(-1..=65535)".to_string(),
                        base: Box::new(AttributeType::Int),
                        validate: validate_to_port_range,
                        namespace: None,
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_description("Not supported. Use IP permissions instead.")
                .with_provider_name("ToPort"),
            )
            .attribute(
                AttributeSchema::new("security_group_rule_id", AttributeType::String)
                    .with_description("The ID of the security group rule. (read-only)")
                    .with_provider_name("SecurityGroupRuleId"),
            ),
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
