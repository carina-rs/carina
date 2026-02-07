//! security_group_ingress schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SecurityGroupIngress
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2_security_group_ingress (AWS::EC2::SecurityGroupIngress)
pub fn ec2_security_group_ingress_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroupIngress",
        resource_type_name: "ec2_security_group_ingress",
        has_tags: false,
        schema: ResourceSchema::new("awscc.ec2_security_group_ingress")
        .with_description("Resource Type definition for AWS::EC2::SecurityGroupIngress")
        .attribute(
            AttributeSchema::new("cidr_ip", AttributeType::String)
                .with_description("The IPv4 ranges")
                .with_provider_name("CidrIp"),
        )
        .attribute(
            AttributeSchema::new("cidr_ipv6", AttributeType::String)
                .with_description("[VPC only] The IPv6 ranges")
                .with_provider_name("CidrIpv6"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_description("Updates the description of an ingress (inbound) security group rule. You can replace an existing description, or add a description to a rule that did ...")
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("from_port", AttributeType::Int)
                .with_description("The start of port range for the TCP and UDP protocols, or an ICMP/ICMPv6 type number. A value of -1 indicates all ICMP/ICMPv6 types. If you specify al...")
                .with_provider_name("FromPort"),
        )
        .attribute(
            AttributeSchema::new("group_id", AttributeType::String)
                .with_description("The ID of the security group. You must specify either the security group ID or the security group name in the request. For security groups in a nondef...")
                .with_provider_name("GroupId"),
        )
        .attribute(
            AttributeSchema::new("group_name", AttributeType::String)
                .with_description("The name of the security group.")
                .with_provider_name("GroupName"),
        )
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("The Security Group Rule Id (read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("ip_protocol", AttributeType::String)
                .required()
                .with_description("The IP protocol name (tcp, udp, icmp, icmpv6) or number (see Protocol Numbers). [VPC only] Use -1 to specify all protocols. When authorizing security ...")
                .with_provider_name("IpProtocol"),
        )
        .attribute(
            AttributeSchema::new("source_prefix_list_id", AttributeType::String)
                .with_description("[EC2-VPC only] The ID of a prefix list. ")
                .with_provider_name("SourcePrefixListId"),
        )
        .attribute(
            AttributeSchema::new("source_security_group_id", AttributeType::String)
                .with_description("The ID of the security group. You must specify either the security group ID or the security group name. For security groups in a nondefault VPC, you m...")
                .with_provider_name("SourceSecurityGroupId"),
        )
        .attribute(
            AttributeSchema::new("source_security_group_name", AttributeType::String)
                .with_description("[EC2-Classic, default VPC] The name of the source security group. You must specify the GroupName property or the GroupId property. For security groups...")
                .with_provider_name("SourceSecurityGroupName"),
        )
        .attribute(
            AttributeSchema::new("source_security_group_owner_id", AttributeType::String)
                .with_description("[nondefault VPC] The AWS account ID that owns the source security group. You can't specify this property with an IP address range. If you specify Sour...")
                .with_provider_name("SourceSecurityGroupOwnerId"),
        )
        .attribute(
            AttributeSchema::new("to_port", AttributeType::Int)
                .with_description("The end of port range for the TCP and UDP protocols, or an ICMP/ICMPv6 code. A value of -1 indicates all ICMP/ICMPv6 codes for the specified ICMP type...")
                .with_provider_name("ToPort"),
        )
    }
}
