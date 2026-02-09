//! security_group schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SecurityGroup
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField, types};

/// Returns the schema config for ec2_security_group (AWS::EC2::SecurityGroup)
pub fn ec2_security_group_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroup",
        resource_type_name: "ec2_security_group",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_security_group")
        .with_description("Resource Type definition for AWS::EC2::SecurityGroup")
        .attribute(
            AttributeSchema::new("group_description", AttributeType::String)
                .required()
                .with_description("A description for the security group.")
                .with_provider_name("GroupDescription"),
        )
        .attribute(
            AttributeSchema::new("group_id", AttributeType::String)
                .with_description("The group ID of the specified security group. (read-only)")
                .with_provider_name("GroupId"),
        )
        .attribute(
            AttributeSchema::new("group_name", AttributeType::String)
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
                    StructField::new("cidr_ip", AttributeType::String).with_provider_name("CidrIp"),
                    StructField::new("cidr_ipv6", types::ipv6_cidr()).with_provider_name("CidrIpv6"),
                    StructField::new("description", AttributeType::String).with_provider_name("Description"),
                    StructField::new("destination_prefix_list_id", AttributeType::String).with_provider_name("DestinationPrefixListId"),
                    StructField::new("destination_security_group_id", AttributeType::String).with_provider_name("DestinationSecurityGroupId"),
                    StructField::new("from_port", AttributeType::Int).with_provider_name("FromPort"),
                    StructField::new("ip_protocol", AttributeType::Enum(vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string(), "icmpv6".to_string(), "-1".to_string()])).required().with_provider_name("IpProtocol"),
                    StructField::new("to_port", AttributeType::Int).with_provider_name("ToPort")
                    ],
                })))
                .with_description("[VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.")
                .with_provider_name("SecurityGroupEgress"),
        )
        .attribute(
            AttributeSchema::new("security_group_ingress", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Ingress".to_string(),
                    fields: vec![
                    StructField::new("cidr_ip", AttributeType::String).with_provider_name("CidrIp"),
                    StructField::new("cidr_ipv6", types::ipv6_cidr()).with_provider_name("CidrIpv6"),
                    StructField::new("description", AttributeType::String).with_provider_name("Description"),
                    StructField::new("from_port", AttributeType::Int).with_provider_name("FromPort"),
                    StructField::new("ip_protocol", AttributeType::Enum(vec!["tcp".to_string(), "udp".to_string(), "icmp".to_string(), "icmpv6".to_string(), "-1".to_string()])).required().with_provider_name("IpProtocol"),
                    StructField::new("source_prefix_list_id", AttributeType::String).with_provider_name("SourcePrefixListId"),
                    StructField::new("source_security_group_id", AttributeType::String).with_provider_name("SourceSecurityGroupId"),
                    StructField::new("source_security_group_name", AttributeType::String).with_provider_name("SourceSecurityGroupName"),
                    StructField::new("source_security_group_owner_id", AttributeType::String).with_provider_name("SourceSecurityGroupOwnerId"),
                    StructField::new("to_port", AttributeType::Int).with_provider_name("ToPort")
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
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("The ID of the VPC for the security group.")
                .with_provider_name("VpcId"),
        )
    }
}
