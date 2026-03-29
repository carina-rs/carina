//! vpc_peering_connection schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.vpc_peering_connection (Smithy: com.amazonaws.ec2)
pub fn ec2_vpc_peering_connection_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::VPCPeeringConnection",
        resource_type_name: "ec2.vpc_peering_connection",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.vpc_peering_connection")
            .with_description("Describes a VPC peering connection.")
            .attribute(
                AttributeSchema::new("peer_owner_id", AttributeType::String)
                    .create_only()
                    .with_description("The AWS account ID of the owner of the accepter VPC.")
                    .with_provider_name("PeerOwnerId"),
            )
            .attribute(
                AttributeSchema::new("peer_region", AttributeType::String)
                    .create_only()
                    .with_description("The Region code for the accepter VPC.")
                    .with_provider_name("PeerRegion"),
            )
            .attribute(
                AttributeSchema::new("peer_vpc_id", super::vpc_id())
                    .required()
                    .create_only()
                    .with_description(
                        "The ID of the VPC with which you are creating the VPC peering connection.",
                    )
                    .with_provider_name("PeerVpcId"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("Any tags assigned to the VPC peering connection.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new("vpc_id", super::vpc_id())
                    .required()
                    .create_only()
                    .with_description("The ID of the VPC.")
                    .with_provider_name("VpcId"),
            )
            .attribute(
                AttributeSchema::new(
                    "vpc_peering_connection_id",
                    super::vpc_peering_connection_id(),
                )
                .with_description("The ID of the VPC peering connection. (read-only)")
                .with_provider_name("VpcPeeringConnectionId"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.vpc_peering_connection", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
