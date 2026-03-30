//! eip schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, types};

/// Returns the schema config for ec2.eip (Smithy: com.amazonaws.ec2)
pub fn ec2_eip_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::EIP",
        resource_type_name: "ec2.eip",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.eip")
            .with_description("Describes an Elastic IP address.")
            .attribute(
                AttributeSchema::new("allocation_id", super::allocation_id())
                    .with_description("The allocation ID for the Elastic IP address. (read-only)")
                    .with_provider_name("AllocationId"),
            )
            .attribute(
                AttributeSchema::new(
                    "domain",
                    AttributeType::StringEnum {
                        name: "Domain".to_string(),
                        values: vec!["vpc".to_string()],
                        namespace: Some("aws.ec2.eip".to_string()),
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_description("The network (vpc).")
                .with_provider_name("Domain"),
            )
            .attribute(
                AttributeSchema::new("public_ip", types::ipv4_address())
                    .with_description("The Elastic IP address. (read-only)")
                    .with_provider_name("PublicIp"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags for the resource.")
                    .with_provider_name("Tags"),
            ),
    }
}

const VALID_DOMAIN: &[&str] = &["vpc"];

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2.eip", &[("domain", VALID_DOMAIN)])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
