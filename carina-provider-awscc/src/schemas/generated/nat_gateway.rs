//! ec2_nat_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::NatGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_nat_gateway (AWS::EC2::NatGateway)
pub fn ec2_nat_gateway_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_nat_gateway")
        .with_description("Specifies a network address translation (NAT) gateway in the specified subnet. You can create either a public NAT gateway or a private NAT gateway. The default is a public NAT gateway. If you create a...")
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the NAT gateway."),
        )
        .attribute(
            AttributeSchema::new("allocation_id", AttributeType::String)
                .with_description("[Public NAT gateway only] The allocation ID of the Elastic IP address that's associated with the NAT gateway. This property is required for a public N..."),
        )
        .attribute(
            AttributeSchema::new("route_table_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("secondary_private_ip_address_count", AttributeType::Int)
                .with_description("[Private NAT gateway only] The number of secondary private IPv4 addresses you want to assign to the NAT gateway. For more information about secondary ..."),
        )
        .attribute(
            AttributeSchema::new("subnet_id", AttributeType::String)
                .with_description("The ID of the subnet in which the NAT gateway is located."),
        )
        .attribute(
            AttributeSchema::new("max_drain_duration_seconds", AttributeType::Int)
                .with_description("The maximum amount of time to wait (in seconds) before forcibly releasing the IP addresses if connections are still in progress. Default value is 350 ..."),
        )
        .attribute(
            AttributeSchema::new("eni_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("connectivity_type", AttributeType::String)
                .with_description("Indicates whether the NAT gateway supports public or private connectivity. The default is public connectivity."),
        )
        .attribute(
            AttributeSchema::new("availability_zone_addresses", AttributeType::List(Box::new(AttributeType::String)))
                .with_description(""),
        )
        .attribute(
            AttributeSchema::new("auto_provision_zones", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("The ID of the VPC in which the NAT gateway is located."),
        )
        .attribute(
            AttributeSchema::new("availability_mode", AttributeType::String)
                .with_description(""),
        )
        .attribute(
            AttributeSchema::new("secondary_private_ip_addresses", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("Secondary private IPv4 addresses. For more information about secondary addresses, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/us..."),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("private_ip_address", AttributeType::String)
                .with_description("The private IPv4 address to assign to the NAT gateway. If you don't provide an address, a private IPv4 address will be automatically assigned."),
        )
        .attribute(
            AttributeSchema::new("secondary_allocation_ids", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("Secondary EIP allocation IDs. For more information, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-wi..."),
        )
        .attribute(
            AttributeSchema::new("auto_scaling_ips", AttributeType::String)
                .with_description(" (read-only)"),
        )
}
