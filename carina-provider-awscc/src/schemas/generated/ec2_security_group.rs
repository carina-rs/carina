//! security_group schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SecurityGroup
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField, types};

#[allow(dead_code)]
const VALID_IP_PROTOCOL: &[&str] = &["tcp", "udp", "icmp", "icmpv6", "-1", "all"];

#[allow(dead_code)]
fn validate_ip_protocol(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "IpProtocol",
        "awscc.ec2.security_group",
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

/// Returns the schema config for ec2_security_group (AWS::EC2::SecurityGroup)
pub fn ec2_security_group_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroup",
        resource_type_name: "ec2.security_group",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.security_group")
        .with_description("Resource Type definition for AWS::EC2::SecurityGroup")
        .attribute(
            AttributeSchema::new("group_description", AttributeType::String)
                .required()
                .create_only()
                .with_description("A description for the security group.")
                .with_provider_name("GroupDescription"),
        )
        .attribute(
            AttributeSchema::new("group_id", super::security_group_id())
                .with_description("The group ID of the specified security group. (read-only)")
                .with_provider_name("GroupId"),
        )
        .attribute(
            AttributeSchema::new("group_name", AttributeType::String)
                .create_only()
                .with_description("The name of the security group.")
                .with_provider_name("GroupName"),
        )
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("The group name or group ID depending on whether the SG is created in default or specific VPC (read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("security_group_egress", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Egress".to_string(),
                    fields: vec![
                    StructField::new("cidr_ip", types::ipv4_cidr()).with_provider_name("CidrIp"),
                    StructField::new("cidr_ipv6", types::ipv6_cidr()).with_provider_name("CidrIpv6"),
                    StructField::new("description", AttributeType::String).with_provider_name("Description"),
                    StructField::new("destination_prefix_list_id", super::aws_resource_id()).with_provider_name("DestinationPrefixListId"),
                    StructField::new("destination_security_group_id", super::security_group_id()).with_provider_name("DestinationSecurityGroupId"),
                    StructField::new("from_port", AttributeType::Custom {
                name: "Int(-1..=65535)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_from_port_range,
                namespace: None,
                to_dsl: None,
            }).with_provider_name("FromPort"),
                    StructField::new("ip_protocol", AttributeType::Custom {
                name: "IpProtocol".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_ip_protocol,
                namespace: Some("awscc.ec2.security_group".to_string()),
                to_dsl: Some(|s: &str| match s { "-1" => "all".to_string(), _ => s.replace('-', "_") }),
            }).required().with_provider_name("IpProtocol"),
                    StructField::new("to_port", AttributeType::Custom {
                name: "Int(-1..=65535)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_to_port_range,
                namespace: None,
                to_dsl: None,
            }).with_provider_name("ToPort")
                    ],
                })))
                .with_description("[VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.")
                .with_provider_name("SecurityGroupEgress"),
        )
        .attribute(
            AttributeSchema::new("security_group_ingress", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Ingress".to_string(),
                    fields: vec![
                    StructField::new("cidr_ip", types::ipv4_cidr()).with_provider_name("CidrIp"),
                    StructField::new("cidr_ipv6", types::ipv6_cidr()).with_provider_name("CidrIpv6"),
                    StructField::new("description", AttributeType::String).with_provider_name("Description"),
                    StructField::new("from_port", AttributeType::Custom {
                name: "Int(-1..=65535)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_from_port_range,
                namespace: None,
                to_dsl: None,
            }).with_provider_name("FromPort"),
                    StructField::new("ip_protocol", AttributeType::Custom {
                name: "IpProtocol".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_ip_protocol,
                namespace: Some("awscc.ec2.security_group".to_string()),
                to_dsl: Some(|s: &str| match s { "-1" => "all".to_string(), _ => s.replace('-', "_") }),
            }).required().with_provider_name("IpProtocol"),
                    StructField::new("source_prefix_list_id", super::aws_resource_id()).with_provider_name("SourcePrefixListId"),
                    StructField::new("source_security_group_id", super::security_group_id()).with_provider_name("SourceSecurityGroupId"),
                    StructField::new("source_security_group_name", AttributeType::String).with_provider_name("SourceSecurityGroupName"),
                    StructField::new("source_security_group_owner_id", AttributeType::String).with_provider_name("SourceSecurityGroupOwnerId"),
                    StructField::new("to_port", AttributeType::Custom {
                name: "Int(-1..=65535)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_to_port_range,
                namespace: None,
                to_dsl: None,
            }).with_provider_name("ToPort")
                    ],
                })))
                .with_description("The inbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.")
                .with_provider_name("SecurityGroupIngress"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the security group.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .create_only()
                .with_description("The ID of the VPC for the security group.")
                .with_provider_name("VpcId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.security_group", &[("ip_protocol", VALID_IP_PROTOCOL)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    match (attr_name, value) {
        ("ip_protocol", "all") => Some("-1"),
        _ => None,
    }
}
