//! ec2_route_table schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::RouteTable
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_route_table (AWS::EC2::RouteTable)
pub fn ec2_route_table_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_route_table")
        .with_description("Specifies a route table for the specified VPC. After you create a route table, you can add routes and associate the table with a subnet.  For more information, see [Route tables](https://docs.aws.amaz...")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("The ID of the VPC."),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the route table."),
        )
        .attribute(
            AttributeSchema::new("route_table_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
}
