//! ipam schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::IPAM
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

const VALID_METERED_ACCOUNT: &[&str] = &["ipam-owner", "resource-owner"];

const VALID_TIER: &[&str] = &["free", "advanced"];

fn validate_string_length_min_1(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        let len = s.chars().count();
        if len < 1 {
            Err(format!("String length {} is out of range 1..", len))
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}

fn validate_string_length_max_255(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        let len = s.chars().count();
        if len > 255 {
            Err(format!("String length {} is out of range ..=255", len))
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}

/// Returns the schema config for ec2_ipam (AWS::EC2::IPAM)
pub fn ec2_ipam_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::IPAM",
        resource_type_name: "ec2.ipam",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.ipam")
        .with_description("Resource Schema of AWS::EC2::IPAM Type")
        .attribute(
            AttributeSchema::new("arn", super::arn())
                .with_description("The Amazon Resource Name (ARN) of the IPAM. (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("default_resource_discovery_association_id", AttributeType::String)
                .with_description("The Id of the default association to the default resource discovery, created with this IPAM. (read-only)")
                .with_provider_name("DefaultResourceDiscoveryAssociationId"),
        )
        .attribute(
            AttributeSchema::new("default_resource_discovery_id", AttributeType::String)
                .with_description("The Id of the default resource discovery, created with this IPAM. (read-only)")
                .with_provider_name("DefaultResourceDiscoveryId"),
        )
        .attribute(
            AttributeSchema::new("default_resource_discovery_organizational_unit_exclusions", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "IpamOrganizationalUnitExclusion".to_string(),
                    fields: vec![
                    StructField::new("organizations_entity_path", AttributeType::Custom {
                name: "String(len: 1..)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_min_1,
                namespace: None,
                to_dsl: None,
            }).required().with_description("An AWS Organizations entity path. Build the path for the OU(s) using AWS Organizations IDs separated by a '/'. Include all child OUs by ending the pat...").with_provider_name("OrganizationsEntityPath")
                    ],
                })))
                .with_description("A set of organizational unit (OU) exclusions for the default resource discovery, created with this IPAM.")
                .with_provider_name("DefaultResourceDiscoveryOrganizationalUnitExclusions")
                .with_block_name("default_resource_discovery_organizational_unit_exclusion"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("enable_private_gua", AttributeType::Bool)
                .with_description("Enable provisioning of GUA space in private pools.")
                .with_provider_name("EnablePrivateGua"),
        )
        .attribute(
            AttributeSchema::new("ipam_id", super::ipam_id())
                .with_description("Id of the IPAM. (read-only)")
                .with_provider_name("IpamId"),
        )
        .attribute(
            AttributeSchema::new("metered_account", AttributeType::StringEnum {
                name: "MeteredAccount".to_string(),
                values: vec!["ipam-owner".to_string(), "resource-owner".to_string()],
                namespace: Some("awscc.ec2.ipam".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })
                .with_description("A metered account is an account that is charged for active IP addresses managed in IPAM")
                .with_provider_name("MeteredAccount"),
        )
        .attribute(
            AttributeSchema::new("operating_regions", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "IpamOperatingRegion".to_string(),
                    fields: vec![
                    StructField::new("region_name", super::awscc_region()).required().with_description("The name of the region.").with_provider_name("RegionName")
                    ],
                })))
                .with_description("The regions IPAM is enabled for. Allows pools to be created in these regions, as well as enabling monitoring")
                .with_provider_name("OperatingRegions")
                .with_block_name("operating_region"),
        )
        .attribute(
            AttributeSchema::new("private_default_scope_id", AttributeType::String)
                .with_description("The Id of the default scope for publicly routable IP space, created with this IPAM. (read-only)")
                .with_provider_name("PrivateDefaultScopeId"),
        )
        .attribute(
            AttributeSchema::new("public_default_scope_id", AttributeType::Custom {
                name: "String(len: ..=255)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_255,
                namespace: None,
                to_dsl: None,
            })
                .with_description("The Id of the default scope for publicly routable IP space, created with this IPAM. (read-only)")
                .with_provider_name("PublicDefaultScopeId"),
        )
        .attribute(
            AttributeSchema::new("resource_discovery_association_count", AttributeType::Int)
                .with_description("The count of resource discoveries associated with this IPAM. (read-only)")
                .with_provider_name("ResourceDiscoveryAssociationCount"),
        )
        .attribute(
            AttributeSchema::new("scope_count", AttributeType::Int)
                .with_description("The number of scopes that currently exist in this IPAM. (read-only)")
                .with_provider_name("ScopeCount"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("An array of key-value pairs to apply to this resource.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("tier", AttributeType::StringEnum {
                name: "Tier".to_string(),
                values: vec!["free".to_string(), "advanced".to_string()],
                namespace: Some("awscc.ec2.ipam".to_string()),
                to_dsl: None,
            })
                .with_description("The tier of the IPAM.")
                .with_provider_name("Tier"),
        )
        .force_replace()
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.ipam",
        &[
            ("metered_account", VALID_METERED_ACCOUNT),
            ("tier", VALID_TIER),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
