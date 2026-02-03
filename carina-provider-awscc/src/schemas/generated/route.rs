//! ec2_route schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::Route
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Returns the schema for ec2_route (AWS::EC2::Route)
pub fn ec2_route_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_route")
        .with_description("Specifies a route in a route table. For more information, see [Routes](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Route_Tables.html#route-table-routes) in the *Amazon VPC User Guide*.  You m...")
        .attribute(
            AttributeSchema::new("local_gateway_id", AttributeType::String)
                .with_description("The ID of the local gateway."),
        )
        .attribute(
            AttributeSchema::new("destination_ipv6_cidr_block", types::cidr())
                .with_description("The IPv6 CIDR block used for the destination match. Routing decisions are based on the most specific match."),
        )
        .attribute(
            AttributeSchema::new("vpc_peering_connection_id", AttributeType::String)
                .with_description("The ID of a VPC peering connection."),
        )
        .attribute(
            AttributeSchema::new("destination_cidr_block", types::cidr())
                .with_description("The IPv4 CIDR address block used for the destination match. Routing decisions are based on the most specific match. We modify the specified CIDR block..."),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", AttributeType::String)
                .with_description("[IPv4 traffic only] The ID of a NAT gateway."),
        )
        .attribute(
            AttributeSchema::new("transit_gateway_id", AttributeType::String)
                .with_description("The ID of a transit gateway."),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::cidr())
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("carrier_gateway_id", AttributeType::String)
                .with_description("The ID of the carrier gateway. You can only use this option when the VPC contains a subnet which is associated with a Wavelength Zone."),
        )
        .attribute(
            AttributeSchema::new("destination_prefix_list_id", AttributeType::String)
                .with_description("The ID of a prefix list used for the destination match."),
        )
        .attribute(
            AttributeSchema::new("route_table_id", AttributeType::String)
                .required()
                .with_description("The ID of the route table for the route."),
        )
        .attribute(
            AttributeSchema::new("core_network_arn", AttributeType::String)
                .with_description("The Amazon Resource Name (ARN) of the core network."),
        )
        .attribute(
            AttributeSchema::new("gateway_id", AttributeType::String)
                .with_description("The ID of an internet gateway or virtual private gateway attached to your VPC."),
        )
        .attribute(
            AttributeSchema::new("instance_id", AttributeType::String)
                .with_description("The ID of a NAT instance in your VPC. The operation fails if you specify an instance ID unless exactly one network interface is attached."),
        )
        .attribute(
            AttributeSchema::new("network_interface_id", AttributeType::String)
                .with_description("The ID of a network interface."),
        )
        .attribute(
            AttributeSchema::new("vpc_endpoint_id", AttributeType::String)
                .with_description("The ID of a VPC endpoint. Supported for Gateway Load Balancer endpoints only."),
        )
        .attribute(
            AttributeSchema::new("egress_only_internet_gateway_id", AttributeType::String)
                .with_description("[IPv6 traffic only] The ID of an egress-only internet gateway."),
        )
}
