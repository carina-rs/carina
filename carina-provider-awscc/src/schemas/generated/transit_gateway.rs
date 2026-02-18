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

const VALID_AUTO_ACCEPT_SHARED_ATTACHMENTS: &[&str] = &["enable", "disable"];

fn validate_auto_accept_shared_attachments(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "AutoAcceptSharedAttachments",
        "awscc.ec2_transit_gateway",
        VALID_AUTO_ACCEPT_SHARED_ATTACHMENTS,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid AutoAcceptSharedAttachments '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_DEFAULT_ROUTE_TABLE_ASSOCIATION: &[&str] = &["enable", "disable"];

fn validate_default_route_table_association(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "DefaultRouteTableAssociation",
        "awscc.ec2_transit_gateway",
        VALID_DEFAULT_ROUTE_TABLE_ASSOCIATION,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid DefaultRouteTableAssociation '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_DEFAULT_ROUTE_TABLE_PROPAGATION: &[&str] = &["enable", "disable"];

fn validate_default_route_table_propagation(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "DefaultRouteTablePropagation",
        "awscc.ec2_transit_gateway",
        VALID_DEFAULT_ROUTE_TABLE_PROPAGATION,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid DefaultRouteTablePropagation '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_DNS_SUPPORT: &[&str] = &["enable", "disable"];

fn validate_dns_support(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "DnsSupport",
        "awscc.ec2_transit_gateway",
        VALID_DNS_SUPPORT,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid DnsSupport '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_ENCRYPTION_SUPPORT: &[&str] = &["disable", "enable"];

fn validate_encryption_support(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "EncryptionSupport",
        "awscc.ec2_transit_gateway",
        VALID_ENCRYPTION_SUPPORT,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid EncryptionSupport '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_MULTICAST_SUPPORT: &[&str] = &["enable", "disable"];

fn validate_multicast_support(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "MulticastSupport",
        "awscc.ec2_transit_gateway",
        VALID_MULTICAST_SUPPORT,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid MulticastSupport '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_SECURITY_GROUP_REFERENCING_SUPPORT: &[&str] = &["enable", "disable"];

fn validate_security_group_referencing_support(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "SecurityGroupReferencingSupport",
        "awscc.ec2_transit_gateway",
        VALID_SECURITY_GROUP_REFERENCING_SUPPORT,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!(
                "Invalid SecurityGroupReferencingSupport '{}': {}",
                s, reason
            )
        } else {
            reason
        }
    })
}

const VALID_VPN_ECMP_SUPPORT: &[&str] = &["enable", "disable"];

fn validate_vpn_ecmp_support(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "VpnEcmpSupport",
        "awscc.ec2_transit_gateway",
        VALID_VPN_ECMP_SUPPORT,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid VpnEcmpSupport '{}': {}", s, reason)
        } else {
            reason
        }
    })
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
                    .create_only()
                    .with_provider_name("AmazonSideAsn"),
            )
            .attribute(
                AttributeSchema::new(
                    "association_default_route_table_id",
                    super::route_table_id(),
                )
                .with_provider_name("AssociationDefaultRouteTableId"),
            )
            .attribute(
                AttributeSchema::new(
                    "auto_accept_shared_attachments",
                    AttributeType::Custom {
                        name: "AutoAcceptSharedAttachments".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_auto_accept_shared_attachments,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("AutoAcceptSharedAttachments"),
            )
            .attribute(
                AttributeSchema::new(
                    "default_route_table_association",
                    AttributeType::Custom {
                        name: "DefaultRouteTableAssociation".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_default_route_table_association,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .with_provider_name("DefaultRouteTableAssociation"),
            )
            .attribute(
                AttributeSchema::new(
                    "default_route_table_propagation",
                    AttributeType::Custom {
                        name: "DefaultRouteTablePropagation".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_default_route_table_propagation,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
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
                    AttributeType::Custom {
                        name: "DnsSupport".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_dns_support,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
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
                        to_dsl: None,
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
                AttributeSchema::new(
                    "multicast_support",
                    AttributeType::Custom {
                        name: "MulticastSupport".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_multicast_support,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_provider_name("MulticastSupport"),
            )
            .attribute(
                AttributeSchema::new(
                    "propagation_default_route_table_id",
                    super::route_table_id(),
                )
                .with_provider_name("PropagationDefaultRouteTableId"),
            )
            .attribute(
                AttributeSchema::new(
                    "security_group_referencing_support",
                    AttributeType::Custom {
                        name: "SecurityGroupReferencingSupport".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_security_group_referencing_support,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
                        to_dsl: None,
                    },
                )
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
                AttributeSchema::new(
                    "vpn_ecmp_support",
                    AttributeType::Custom {
                        name: "VpnEcmpSupport".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_vpn_ecmp_support,
                        namespace: Some("awscc.ec2_transit_gateway".to_string()),
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
        "ec2_transit_gateway",
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
            ("multicast_support", VALID_MULTICAST_SUPPORT),
            (
                "security_group_referencing_support",
                VALID_SECURITY_GROUP_REFERENCING_SUPPORT,
            ),
            ("vpn_ecmp_support", VALID_VPN_ECMP_SUPPORT),
        ],
    )
}
