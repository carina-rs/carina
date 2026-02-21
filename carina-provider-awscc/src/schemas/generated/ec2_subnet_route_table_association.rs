//! subnet_route_table_association schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SubnetRouteTableAssociation
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2_subnet_route_table_association (AWS::EC2::SubnetRouteTableAssociation)
pub fn ec2_subnet_route_table_association_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::SubnetRouteTableAssociation",
        resource_type_name: "ec2_subnet_route_table_association",
        has_tags: false,
        schema: ResourceSchema::new("awscc.ec2_subnet_route_table_association")
        .with_description("Associates a subnet with a route table. The subnet and route table must be in the same VPC. This association causes traffic originating from the subnet to be routed according to the routes in the rout...")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", super::route_table_id())
                .required()
                .create_only()
                .with_description("The ID of the route table. The physical ID changes when the route table ID is changed.")
                .with_provider_name("RouteTableId"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", super::subnet_id())
                .required()
                .create_only()
                .with_description("The ID of the subnet.")
                .with_provider_name("SubnetId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_subnet_route_table_association", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
