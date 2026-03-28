//! transit_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::TransitGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

const VALID_AUTO_ACCEPT_SHARED_ATTACHMENTS: &[&str] = &["enable", "disable"];

const VALID_DEFAULT_ROUTE_TABLE_ASSOCIATION: &[&str] = &["enable", "disable"];

const VALID_DEFAULT_ROUTE_TABLE_PROPAGATION: &[&str] = &["enable", "disable"];

const VALID_DNS_SUPPORT: &[&str] = &["enable", "disable"];

const VALID_ENCRYPTION_SUPPORT: &[&str] = &["disable", "enable"];

const VALID_ENCRYPTION_SUPPORT_STATE: &[&str] = &["disable", "enable"];

const VALID_MULTICAST_SUPPORT: &[&str] = &["enable", "disable"];

const VALID_SECURITY_GROUP_REFERENCING_SUPPORT: &[&str] = &["enable", "disable"];

const VALID_VPN_ECMP_SUPPORT: &[&str] = &["enable", "disable"];

fn validate_amazon_side_asn_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 1 || *n > 4294967294 {
            Err(format!("Value {} is out of range 1..=4294967294", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for ec2_transit_gateway (AWS::EC2::TransitGateway)
pub fn ec2_transit_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::TransitGateway",
        resource_type_name: "ec2.transit_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.transit_gateway")
            .with_description("Resource Type definition for AWS::EC2::TransitGateway")
            .attribute(
                AttributeSchema::new(
                    "amazon_side_asn",
                    AttributeType::Custom {
                        name: "Int(1..=4294967294)".to_string(),
                        base: Box::new(AttributeType::Int),
                        validate: validate_amazon_side_asn_range,
                        namespace: None,
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_provider_name("AmazonSideAsn"),
            )
            .attribute(
                AttributeSchema::new(
                    "association_default_route_table_id",
                    super::tgw_route_table_id(),
                )
                .with_provider_name("AssociationDefaultRouteTableId"),
            )
            .attribute(
                AttributeSchema::new(
                    "auto_accept_shared_attachments",
                    AttributeType::StringEnum {
                        name: "AutoAcceptSharedAttachments".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("AutoAcceptSharedAttachments"),
            )
            .attribute(
                AttributeSchema::new(
                    "default_route_table_association",
                    AttributeType::StringEnum {
                        name: "DefaultRouteTableAssociation".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("DefaultRouteTableAssociation"),
            )
            .attribute(
                AttributeSchema::new(
                    "default_route_table_propagation",
                    AttributeType::StringEnum {
                        name: "DefaultRouteTablePropagation".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("DefaultRouteTablePropagation"),
            )
            .attribute(
                AttributeSchema::new("description", AttributeType::String)
                    .with_provider_name("Description"),
            )
            .attribute(
                AttributeSchema::new(
                    "dns_support",
                    AttributeType::StringEnum {
                        name: "DnsSupport".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("DnsSupport"),
            )
            .attribute(
                AttributeSchema::new(
                    "encryption_support",
                    AttributeType::StringEnum {
                        name: "EncryptionSupport".to_string(),
                        values: vec!["disable".to_string(), "enable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .write_only()
                .with_provider_name("EncryptionSupport"),
            )
            .attribute(
                AttributeSchema::new(
                    "encryption_support_state",
                    AttributeType::StringEnum {
                        name: "EncryptionSupportState".to_string(),
                        values: vec!["disable".to_string(), "enable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .read_only()
                .with_provider_name("EncryptionSupportState"),
            )
            .attribute(
                AttributeSchema::new("id", super::transit_gateway_id())
                    .read_only()
                    .with_provider_name("Id"),
            )
            .attribute(
                AttributeSchema::new(
                    "multicast_support",
                    AttributeType::StringEnum {
                        name: "MulticastSupport".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_provider_name("MulticastSupport"),
            )
            .attribute(
                AttributeSchema::new(
                    "propagation_default_route_table_id",
                    super::tgw_route_table_id(),
                )
                .with_provider_name("PropagationDefaultRouteTableId"),
            )
            .attribute(
                AttributeSchema::new(
                    "security_group_referencing_support",
                    AttributeType::StringEnum {
                        name: "SecurityGroupReferencingSupport".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("SecurityGroupReferencingSupport"),
            )
            .attribute(AttributeSchema::new("tags", tags_type()).with_provider_name("Tags"))
            .attribute(
                AttributeSchema::new("transit_gateway_arn", super::arn())
                    .read_only()
                    .with_provider_name("TransitGatewayArn"),
            )
            .attribute(
                AttributeSchema::new(
                    "transit_gateway_cidr_blocks",
                    AttributeType::list(types::cidr()),
                )
                .with_provider_name("TransitGatewayCidrBlocks"),
            )
            .attribute(
                AttributeSchema::new(
                    "vpn_ecmp_support",
                    AttributeType::StringEnum {
                        name: "VpnEcmpSupport".to_string(),
                        values: vec!["enable".to_string(), "disable".to_string()],
                        namespace: Some("awscc.ec2.transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
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
            ("encryption_support", VALID_ENCRYPTION_SUPPORT),
            ("encryption_support_state", VALID_ENCRYPTION_SUPPORT_STATE),
            ("multicast_support", VALID_MULTICAST_SUPPORT),
            (
                "security_group_referencing_support",
                VALID_SECURITY_GROUP_REFERENCING_SUPPORT,
            ),
            ("vpn_ecmp_support", VALID_VPN_ECMP_SUPPORT),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
