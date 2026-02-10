//! subnet_route_table_association schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SubnetRouteTableAssociation
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

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
            AttributeSchema::new("route_table_id", types::aws_resource_id())
                .required()
                .with_description("The ID of the route table. The physical ID changes when the route table ID is changed.")
                .with_provider_name("RouteTableId"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", types::aws_resource_id())
                .required()
                .with_description("The ID of the subnet.")
                .with_provider_name("SubnetId"),
        )
    }
}
