//! route schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::Route
//!
//! DO NOT EDIT MANUALLY - regenerate with aws-codegen

use super::AwsSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Returns the schema config for ec2_route (AWS::EC2::Route)
pub fn ec2_route_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::Route",
        resource_type_name: "ec2_route",
        has_tags: false,
        schema: ResourceSchema::new("aws.ec2_route")
        .with_description("Specifies a route in a route table. For more information, see [Routes](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Route_Tables.html#route-table-routes) in the *Amazon VPC User Guide*.  You m...")
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .with_description("Resource name"),
        )
        .attribute(
            AttributeSchema::new("region", super::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("carrier_gateway_id", super::aws_resource_id())
                .with_description("The ID of the carrier gateway. You can only use this option when the VPC contains a subnet which is associated with a Wavelength Zone.")
                .with_provider_name("CarrierGatewayId"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::ipv4_cidr())
                .with_description(" (read-only)")
                .with_provider_name("CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("core_network_arn", super::arn())
                .with_description("The Amazon Resource Name (ARN) of the core network.")
                .with_provider_name("CoreNetworkArn"),
        )
        .attribute(
            AttributeSchema::new("destination_cidr_block", types::ipv4_cidr())
                .create_only()
                .with_description("The IPv4 CIDR address block used for the destination match. Routing decisions are based on the most specific match. We modify the specified CIDR block...")
                .with_provider_name("DestinationCidrBlock"),
        )
        .attribute(
            AttributeSchema::new("destination_ipv6_cidr_block", types::ipv6_cidr())
                .create_only()
                .with_description("The IPv6 CIDR block used for the destination match. Routing decisions are based on the most specific match.")
                .with_provider_name("DestinationIpv6CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("destination_prefix_list_id", super::aws_resource_id())
                .create_only()
                .with_description("The ID of a prefix list used for the destination match.")
                .with_provider_name("DestinationPrefixListId"),
        )
        .attribute(
            AttributeSchema::new("egress_only_internet_gateway_id", super::egress_only_internet_gateway_id())
                .with_description("[IPv6 traffic only] The ID of an egress-only internet gateway.")
                .with_provider_name("EgressOnlyInternetGatewayId"),
        )
        .attribute(
            AttributeSchema::new("gateway_id", super::aws_resource_id())
                .with_description("The ID of an internet gateway or virtual private gateway attached to your VPC.")
                .with_provider_name("GatewayId"),
        )
        .attribute(
            AttributeSchema::new("instance_id", super::aws_resource_id())
                .with_description("The ID of a NAT instance in your VPC. The operation fails if you specify an instance ID unless exactly one network interface is attached.")
                .with_provider_name("InstanceId"),
        )
        .attribute(
            AttributeSchema::new("local_gateway_id", super::aws_resource_id())
                .with_description("The ID of the local gateway.")
                .with_provider_name("LocalGatewayId"),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", super::nat_gateway_id())
                .with_description("[IPv4 traffic only] The ID of a NAT gateway.")
                .with_provider_name("NatGatewayId"),
        )
        .attribute(
            AttributeSchema::new("network_interface_id", super::aws_resource_id())
                .with_description("The ID of a network interface.")
                .with_provider_name("NetworkInterfaceId"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", super::route_table_id())
                .required()
                .create_only()
                .with_description("The ID of the route table for the route.")
                .with_provider_name("RouteTableId"),
        )
        .attribute(
            AttributeSchema::new("transit_gateway_id", super::transit_gateway_id())
                .with_description("The ID of a transit gateway.")
                .with_provider_name("TransitGatewayId"),
        )
        .attribute(
            AttributeSchema::new("vpc_endpoint_id", super::vpc_endpoint_id())
                .with_description("The ID of a VPC endpoint. Supported for Gateway Load Balancer endpoints only.")
                .with_provider_name("VpcEndpointId"),
        )
        .attribute(
            AttributeSchema::new("vpc_peering_connection_id", super::vpc_peering_connection_id())
                .with_description("The ID of a VPC peering connection.")
                .with_provider_name("VpcPeeringConnectionId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_route", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
