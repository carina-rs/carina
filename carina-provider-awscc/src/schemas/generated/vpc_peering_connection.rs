//! vpc_peering_connection schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPCPeeringConnection
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2_vpc_peering_connection (AWS::EC2::VPCPeeringConnection)
pub fn ec2_vpc_peering_connection_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::VPCPeeringConnection",
        resource_type_name: "ec2_vpc_peering_connection",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_vpc_peering_connection")
        .with_description("Resource Type definition for AWS::EC2::VPCPeeringConnection")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("(read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("peer_owner_id", AttributeType::String)
                .create_only()
                .with_description("The AWS account ID of the owner of the accepter VPC.")
                .with_provider_name("PeerOwnerId"),
        )
        .attribute(
            AttributeSchema::new("peer_region", AttributeType::String)
                .create_only()
                .with_description("The Region code for the accepter VPC, if the accepter VPC is located in a Region other than the Region in which you make the request.")
                .with_provider_name("PeerRegion"),
        )
        .attribute(
            AttributeSchema::new("peer_role_arn", super::arn())
                .create_only()
                .with_description("The Amazon Resource Name (ARN) of the VPC peer role for the peering connection in another AWS account.")
                .with_provider_name("PeerRoleArn"),
        )
        .attribute(
            AttributeSchema::new("peer_vpc_id", super::vpc_id())
                .required()
                .create_only()
                .with_description("The ID of the VPC with which you are creating the VPC peering connection. You must specify this parameter in the request.")
                .with_provider_name("PeerVpcId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
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
