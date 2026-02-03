//! ec2_vpc schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPC
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Returns the schema for ec2_vpc (AWS::EC2::VPC)
pub fn ec2_vpc_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_vpc")
        .with_description("Specifies a virtual private cloud (VPC).  To add an IPv6 CIDR block to the VPC, see [AWS::EC2::VPCCidrBlock](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpccidrbloc...")
        .attribute(
            AttributeSchema::new("enable_dns_support", AttributeType::Bool)
                .with_description("Indicates whether the DNS resolution is supported for the VPC. If enabled, queries to the Amazon provided DNS server at the 169.254.169.253 IP address..."),
        )
        .attribute(
            AttributeSchema::new("enable_dns_hostnames", AttributeType::Bool)
                .with_description("Indicates whether the instances launched in the VPC get DNS hostnames. If enabled, instances in the VPC get DNS hostnames; otherwise, they do not. Dis..."),
        )
        .attribute(
            AttributeSchema::new("ipv4_netmask_length", AttributeType::Int)
                .with_description("The netmask length of the IPv4 CIDR you want to allocate to this VPC from an Amazon VPC IP Address Manager (IPAM) pool. For more information about IPA..."),
        )
        .attribute(
            AttributeSchema::new("cidr_block_associations", AttributeType::List(Box::new(types::cidr())))
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::cidr())
                .with_description("The IPv4 network range for the VPC, in CIDR notation. For example, ``10.0.0.0/16``. We modify the specified CIDR block to its canonical form; for exam..."),
        )
        .attribute(
            AttributeSchema::new("default_security_group", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the VPC."),
        )
        .attribute(
            AttributeSchema::new("ipv4_ipam_pool_id", AttributeType::String)
                .with_description("The ID of an IPv4 IPAM pool you want to use for allocating this VPC's CIDR. For more information, see [What is IPAM?](https://docs.aws.amazon.com//vpc..."),
        )
        .attribute(
            AttributeSchema::new("default_network_acl", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("instance_tenancy", AttributeType::String)
                .with_description("The allowed tenancy of instances launched into the VPC.  + ``default``: An instance launched into the VPC runs on shared hardware by default, unless y..."),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_blocks", AttributeType::List(Box::new(types::cidr())))
                .with_description(" (read-only)"),
        )
}
