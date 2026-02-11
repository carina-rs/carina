//! transit_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::TransitGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

const VALID_ENCRYPTION_SUPPORT: &[&str] = &["disable", "enable"];

fn validate_encryption_support(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "EncryptionSupport",
        "awscc.ec2_transit_gateway",
        VALID_ENCRYPTION_SUPPORT,
    )
}

/// Returns the schema config for ec2_transit_gateway (AWS::EC2::TransitGateway)
pub fn ec2_transit_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::TransitGateway",
        resource_type_name: "ec2_transit_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_transit_gateway")
            .with_description("Resource Type definition for AWS::EC2::TransitGateway")
            .attribute(
                AttributeSchema::new("amazon_side_asn", AttributeType::Int)
                    .with_provider_name("AmazonSideAsn"),
            )
            .attribute(
                AttributeSchema::new(
                    "association_default_route_table_id",
                    super::aws_resource_id(),
                )
                .with_provider_name("AssociationDefaultRouteTableId"),
            )
            .attribute(
                AttributeSchema::new("auto_accept_shared_attachments", AttributeType::String)
                    .with_provider_name("AutoAcceptSharedAttachments"),
            )
            .attribute(
                AttributeSchema::new("default_route_table_association", AttributeType::String)
                    .with_provider_name("DefaultRouteTableAssociation"),
            )
            .attribute(
                AttributeSchema::new("default_route_table_propagation", AttributeType::String)
                    .with_provider_name("DefaultRouteTablePropagation"),
            )
            .attribute(
                AttributeSchema::new("description", AttributeType::String)
                    .with_provider_name("Description"),
            )
            .attribute(
                AttributeSchema::new("dns_support", AttributeType::String)
                    .with_provider_name("DnsSupport"),
            )
            .attribute(
                AttributeSchema::new(
                    "encryption_support",
                    AttributeType::Custom {
                        name: "EncryptionSupport".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_encryption_support,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
                    },
                )
                .with_provider_name("EncryptionSupport"),
            )
            .attribute(
                AttributeSchema::new("encryption_support_state", AttributeType::String)
                    .with_description("(read-only)")
                    .with_provider_name("EncryptionSupportState"),
            )
            .attribute(
                AttributeSchema::new("id", AttributeType::String)
                    .with_description("(read-only)")
                    .with_provider_name("Id"),
            )
            .attribute(
                AttributeSchema::new("multicast_support", AttributeType::String)
                    .with_provider_name("MulticastSupport"),
            )
            .attribute(
                AttributeSchema::new(
                    "propagation_default_route_table_id",
                    super::aws_resource_id(),
                )
                .with_provider_name("PropagationDefaultRouteTableId"),
            )
            .attribute(
                AttributeSchema::new("security_group_referencing_support", AttributeType::String)
                    .with_provider_name("SecurityGroupReferencingSupport"),
            )
            .attribute(AttributeSchema::new("tags", tags_type()).with_provider_name("Tags"))
            .attribute(
                AttributeSchema::new("transit_gateway_arn", super::arn())
                    .with_description("(read-only)")
                    .with_provider_name("TransitGatewayArn"),
            )
            .attribute(
                AttributeSchema::new(
                    "transit_gateway_cidr_blocks",
                    AttributeType::List(Box::new(types::ipv4_cidr())),
                )
                .with_provider_name("TransitGatewayCidrBlocks"),
            )
            .attribute(
                AttributeSchema::new("vpn_ecmp_support", AttributeType::String)
                    .with_provider_name("VpnEcmpSupport"),
            ),
    }
}
