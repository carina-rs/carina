//! route_table schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::RouteTable
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, ResourceSchema};

/// Returns the schema config for ec2_route_table (AWS::EC2::RouteTable)
pub fn ec2_route_table_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::RouteTable",
        resource_type_name: "ec2_route_table",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_route_table")
        .with_description("Specifies a route table for the specified VPC. After you create a route table, you can add routes and associate the table with a subnet.  For more information, see [Route tables](https://docs.aws.amaz...")
        .attribute(
            AttributeSchema::new("route_table_id", super::route_table_id())
                .with_description(" (read-only)")
                .with_provider_name("RouteTableId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the route table.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .required()
                .create_only()
                .with_description("The ID of the VPC.")
                .with_provider_name("VpcId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_route_table", &[])
}
