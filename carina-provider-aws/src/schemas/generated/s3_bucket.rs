//! bucket schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.s3
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

#[allow(dead_code)]
const VALID_VERSIONING_STATUS: &[&str] = &["Enabled", "Suspended"];

#[allow(dead_code)]
fn validate_versioning_status(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "VersioningStatus",
        "aws.s3.bucket",
        VALID_VERSIONING_STATUS,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid VersioningStatus '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

/// Returns the schema config for s3.bucket (Smithy: com.amazonaws.s3)
pub fn s3_bucket_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::S3::Bucket",
        resource_type_name: "s3.bucket",
        has_tags: true,
        schema: ResourceSchema::new("aws.s3.bucket")
            .attribute(
                AttributeSchema::new("name", AttributeType::String)
                    .with_description("Resource name"),
            )
            .attribute(
                AttributeSchema::new("region", super::aws_region())
                    .with_description("The AWS region (inherited from provider if not specified)"),
            )
            .attribute(
                AttributeSchema::new(
                    "versioning_status",
                    AttributeType::Custom {
                        name: "VersioningStatus".to_string(),
                        base: Box::new(AttributeType::String),
                        validate: validate_versioning_status,
                        namespace: Some("aws.s3.bucket".to_string()),
                        to_dsl: None,
                    },
                )
                .with_description("The versioning state of the bucket.")
                .with_provider_name("VersioningStatus"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags for the resource.")
                    .with_provider_name("Tags"),
            ),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "s3.bucket",
        &[("versioning_status", VALID_VERSIONING_STATUS)],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
