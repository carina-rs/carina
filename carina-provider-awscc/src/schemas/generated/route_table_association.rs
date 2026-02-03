//! subnet_route_table_association schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SubnetRouteTableAssociation
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_subnet_route_table_association (AWS::EC2::SubnetRouteTableAssociation)
pub fn ec2_subnet_route_table_association_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_subnet_route_table_association")
        .with_description("Associates a subnet with a route table. The subnet and route table must be in the same VPC. This association causes traffic originating from the subnet to be routed according to the routes in the rout...")
        .attribute(
            AttributeSchema::new("subnet_id", AttributeType::String)
                .required()
                .with_description("The ID of the subnet."),
        )
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", AttributeType::String)
                .required()
                .with_description("The ID of the route table. The physical ID changes when the route table ID is changed."),
        )
}
