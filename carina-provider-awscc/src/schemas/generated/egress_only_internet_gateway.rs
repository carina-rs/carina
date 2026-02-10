//! egress_only_internet_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::EgressOnlyInternetGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Returns the schema config for ec2_egress_only_internet_gateway (AWS::EC2::EgressOnlyInternetGateway)
pub fn ec2_egress_only_internet_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::EgressOnlyInternetGateway",
        resource_type_name: "ec2_egress_only_internet_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_egress_only_internet_gateway")
            .with_description("Resource Type definition for AWS::EC2::EgressOnlyInternetGateway")
            .attribute(
                AttributeSchema::new("id", AttributeType::String)
                    .with_description(
                        "Service Generated ID of the EgressOnlyInternetGateway (read-only)",
                    )
                    .with_provider_name("Id"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("Any tags assigned to the egress only internet gateway.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new("vpc_id", types::aws_resource_id())
                    .required()
                    .with_description(
                        "The ID of the VPC for which to create the egress-only internet gateway.",
                    )
                    .with_provider_name("VpcId"),
            ),
    }
}
