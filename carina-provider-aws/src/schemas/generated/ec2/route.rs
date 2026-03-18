//! route schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use carina_core::schema::{AttributeSchema, ResourceSchema, types};

/// Returns the schema config for ec2.route (Smithy: com.amazonaws.ec2)
pub fn ec2_route_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::Route",
        resource_type_name: "ec2.route",
        has_tags: false,
        schema: ResourceSchema::new("aws.ec2.route")
        .with_description("Describes a route in a route table.")
        .attribute(
            AttributeSchema::new("destination_cidr_block", types::ipv4_cidr())
                .create_only()
                .with_description("The IPv4 CIDR address block used for the destination match. Routing decisions are based on the most specific match. We modify the specified CIDR block...")
                .with_provider_name("DestinationCidrBlock"),
        )
        .attribute(
            AttributeSchema::new("gateway_id", super::gateway_id())
                .with_description("The ID of an internet gateway or virtual private gateway attached to your VPC.")
                .with_provider_name("GatewayId"),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", super::nat_gateway_id())
                .with_description("[IPv4 traffic only] The ID of a NAT gateway.")
                .with_provider_name("NatGatewayId"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", super::route_table_id())
                .required()
                .create_only()
                .with_description("The ID of the route table for the route.")
                .with_provider_name("RouteTableId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.route", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
