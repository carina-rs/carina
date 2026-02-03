//! ec2_internet_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::InternetGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_internet_gateway (AWS::EC2::InternetGateway)
pub fn ec2_internet_gateway_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_internet_gateway")
        .with_description("Allocates an internet gateway for use with a VPC. After creating the Internet gateway, you then attach it to a VPC.")
        .attribute(
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags to assign to the internet gateway."),
        )
}
