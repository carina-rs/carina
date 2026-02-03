//! security_group schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::SecurityGroup
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_security_group (AWS::EC2::SecurityGroup)
pub fn ec2_security_group_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_security_group")
        .with_description("Resource Type definition for AWS::EC2::SecurityGroup")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("The group name or group ID depending on whether the SG is created in default or specific VPC (read-only)"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("The ID of the VPC for the security group."),
        )
        .attribute(
            AttributeSchema::new("security_group_egress", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("[VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group."),
        )
        .attribute(
            AttributeSchema::new("group_id", AttributeType::String)
                .with_description("The group ID of the specified security group. (read-only)"),
        )
        .attribute(
            AttributeSchema::new("group_description", AttributeType::String)
                .required()
                .with_description("A description for the security group."),
        )
        .attribute(
            AttributeSchema::new("group_name", AttributeType::String)
                .with_description("The name of the security group."),
        )
        .attribute(
            AttributeSchema::new("security_group_ingress", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("The inbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group."),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the security group."),
        )
}
