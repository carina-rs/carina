//! ec2_subnet schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::Subnet
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Returns the schema for ec2_subnet (AWS::EC2::Subnet)
pub fn ec2_subnet_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_subnet")
        .with_description("Specifies a subnet for the specified VPC.  For an IPv4 only subnet, specify an IPv4 CIDR block. If the VPC has an IPv6 CIDR block, you can create an IPv6 only subnet or a dual stack subnet instead. Fo...")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("The ID of the VPC the subnet is in. If you update this property, you must also update the ``CidrBlock`` property."),
        )
        .attribute(
            AttributeSchema::new("ipv6_ipam_pool_id", AttributeType::String)
                .with_description("An IPv6 IPAM pool ID for the subnet."),
        )
        .attribute(
            AttributeSchema::new("block_public_access_states", AttributeType::Map(Box::new(AttributeType::String)))
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("network_acl_association_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("enable_lni_at_device_index", AttributeType::Int)
                .with_description("Indicates the device position for local network interfaces in this subnet. For example, ``1`` indicates local network interfaces in this subnet are th..."),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_blocks", AttributeType::List(Box::new(types::cidr())))
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("outpost_arn", AttributeType::String)
                .with_description("The Amazon Resource Name (ARN) of the Outpost."),
        )
        .attribute(
            AttributeSchema::new("ipv6_native", AttributeType::Bool)
                .with_description("Indicates whether this is an IPv6 only subnet. For more information, see [Subnet basics](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Subnets...."),
        )
        .attribute(
            AttributeSchema::new("private_dns_name_options_on_launch", AttributeType::Map(Box::new(AttributeType::String)))
                .with_description("The hostname type for EC2 instances launched into this subnet and how DNS A and AAAA record queries to the instances should be handled. For more infor..."),
        )
        .attribute(
            AttributeSchema::new("ipv4_netmask_length", AttributeType::Int)
                .with_description("An IPv4 netmask length for the subnet."),
        )
        .attribute(
            AttributeSchema::new("ipv4_ipam_pool_id", AttributeType::String)
                .with_description("An IPv4 IPAM pool ID for the subnet."),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the subnet."),
        )
        .attribute(
            AttributeSchema::new("enable_dns64", AttributeType::Bool)
                .with_description("Indicates whether DNS queries made to the Amazon-provided DNS Resolver in this subnet should return synthetic IPv6 addresses for IPv4-only destination..."),
        )
        .attribute(
            AttributeSchema::new("ipv6_netmask_length", AttributeType::Int)
                .with_description("An IPv6 netmask length for the subnet."),
        )
        .attribute(
            AttributeSchema::new("map_public_ip_on_launch", AttributeType::Bool)
                .with_description("Indicates whether instances launched in this subnet receive a public IPv4 address. The default value is ``false``. AWS charges for all public IPv4 add..."),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::cidr())
                .with_description("The IPv4 CIDR block assigned to the subnet. If you update this property, we create a new subnet, and then delete the existing one."),
        )
        .attribute(
            AttributeSchema::new("availability_zone_id", AttributeType::String)
                .with_description("The AZ ID of the subnet."),
        )
        .attribute(
            AttributeSchema::new("availability_zone", AttributeType::String)
                .with_description("The Availability Zone of the subnet. If you update this property, you must also update the ``CidrBlock`` property."),
        )
        .attribute(
            AttributeSchema::new("assign_ipv6_address_on_creation", AttributeType::Bool)
                .with_description("Indicates whether a network interface created in this subnet receives an IPv6 address. The default value is ``false``. If you specify ``AssignIpv6Addr..."),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_block", types::cidr())
                .with_description("The IPv6 CIDR block. If you specify ``AssignIpv6AddressOnCreation``, you must also specify an IPv6 CIDR block."),
        )
}
