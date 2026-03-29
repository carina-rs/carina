//! transit_gateway schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

const VALID_AUTO_ACCEPT_SHARED_ATTACHMENTS: &[&str] = &["enable", "disable"];

const VALID_DEFAULT_ROUTE_TABLE_ASSOCIATION: &[&str] = &["enable", "disable"];

const VALID_DEFAULT_ROUTE_TABLE_PROPAGATION: &[&str] = &["enable", "disable"];

const VALID_DNS_SUPPORT: &[&str] = &["enable", "disable"];

const VALID_VPN_ECMP_SUPPORT: &[&str] = &["enable", "disable"];

/// Returns the schema config for ec2.transit_gateway (Smithy: com.amazonaws.ec2)
pub fn ec2_transit_gateway_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::TransitGateway",
        resource_type_name: "ec2.transit_gateway",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.transit_gateway")
            .with_description("Describes a transit gateway.")
            .attribute(
                AttributeSchema::new("amazon_side_asn", AttributeType::Int)
                    .create_only()
                    .with_description(
                        "The private Autonomous System Number (ASN) for the Amazon side of a BGP session.",
                    )
                    .with_provider_name("AmazonSideAsn"),
            )
            .attribute(
                AttributeSchema::new(
                    "auto_accept_shared_attachments",
                    AttributeType::StringEnum {
                        name: "AutoAcceptSharedAttachments".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("aws.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_description("Enable or disable automatic acceptance of attachment requests.")
                .with_provider_name("AutoAcceptSharedAttachments"),
            )
            .attribute(
                AttributeSchema::new(
                    "default_route_table_association",
                    AttributeType::StringEnum {
                        name: "DefaultRouteTableAssociation".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("aws.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_description("Enable or disable automatic association with the default association route table.")
                .with_provider_name("DefaultRouteTableAssociation"),
            )
            .attribute(
                AttributeSchema::new(
                    "default_route_table_propagation",
                    AttributeType::StringEnum {
                        name: "DefaultRouteTablePropagation".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("aws.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_description("Enable or disable automatic propagation of routes to the default propagation route table.")
                .with_provider_name("DefaultRouteTablePropagation"),
            )
            .attribute(
                AttributeSchema::new("description", AttributeType::String)
                    .with_description("A description for the transit gateway.")
                    .with_provider_name("Description"),
            )
            .attribute(
                AttributeSchema::new(
                    "dns_support",
                    AttributeType::StringEnum {
                        name: "DnsSupport".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("aws.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_description("Enable or disable DNS support.")
                .with_provider_name("DnsSupport"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("Any tags assigned to the transit gateway.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new("transit_gateway_id", super::transit_gateway_id())
                    .with_description("The ID of the transit gateway. (read-only)")
                    .with_provider_name("TransitGatewayId"),
            )
            .attribute(
                AttributeSchema::new(
                    "vpn_ecmp_support",
                    AttributeType::StringEnum {
                        name: "VpnEcmpSupport".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("aws.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_description("Enable or disable Equal Cost Multipath Protocol support.")
                .with_provider_name("VpnEcmpSupport"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.transit_gateway",
        &[
            (
                "auto_accept_shared_attachments",
                VALID_AUTO_ACCEPT_SHARED_ATTACHMENTS,
            ),
            (
                "default_route_table_association",
                VALID_DEFAULT_ROUTE_TABLE_ASSOCIATION,
            ),
            (
                "default_route_table_propagation",
                VALID_DEFAULT_ROUTE_TABLE_PROPAGATION,
            ),
            ("dns_support", VALID_DNS_SUPPORT),
            ("vpn_ecmp_support", VALID_VPN_ECMP_SUPPORT),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
