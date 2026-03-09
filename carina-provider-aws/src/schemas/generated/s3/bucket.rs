//! bucket schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.s3
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

#![allow(dead_code)]

use super::AwsSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, CompletionValue, ResourceSchema};

const VALID_ACL: &[&str] = &[
    "authenticated-read",
    "private",
    "public-read",
    "public-read-write",
];

fn validate_acl(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(value, "ACL", "aws.s3.bucket", VALID_ACL).map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid ACL '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_OBJECT_OWNERSHIP: &[&str] = &[
    "BucketOwnerEnforced",
    "BucketOwnerPreferred",
    "ObjectWriter",
];

fn validate_object_ownership(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "ObjectOwnership",
        "aws.s3.bucket",
        VALID_OBJECT_OWNERSHIP,
    )
    .map_err(|reason| {
        if let Value::String(s) = value {
            format!("Invalid ObjectOwnership '{}': {}", s, reason)
        } else {
            reason
        }
    })
}

const VALID_VERSIONING_STATUS: &[&str] = &["Enabled", "Suspended"];

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
        data_source: false,
        schema: ResourceSchema::new("aws.s3.bucket")
        .attribute(
            AttributeSchema::new("acl", AttributeType::Custom {
                name: "ACL".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_acl,
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })
                .with_description("The canned ACL to apply to the bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("ACL")
                .with_completions(vec![CompletionValue::new("aws.s3.bucket.ACL.authenticated_read", "authenticated-read"), CompletionValue::new("aws.s3.bucket.ACL.private", "private"), CompletionValue::new("aws.s3.bucket.ACL.public_read", "public-read"), CompletionValue::new("aws.s3.bucket.ACL.public_read_write", "public-read-write")]),
        )
        .attribute(
            AttributeSchema::new("bucket", AttributeType::String)
                .required()
                .create_only()
                .with_description("The name of the bucket to create. General purpose buckets - For information about bucket naming restrictions, see Bucket naming rules in the Amazon S3...")
                .with_provider_name("Bucket"),
        )
        .attribute(
            AttributeSchema::new("grant_full_control", AttributeType::String)
                .with_description("Allows grantee the read, write, read ACP, and write ACP permissions on the bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantFullControl"),
        )
        .attribute(
            AttributeSchema::new("grant_read", AttributeType::String)
                .with_description("Allows grantee to list the objects in the bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantRead"),
        )
        .attribute(
            AttributeSchema::new("grant_read_acp", AttributeType::String)
                .with_description("Allows grantee to read the bucket ACL. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantReadACP"),
        )
        .attribute(
            AttributeSchema::new("grant_write", AttributeType::String)
                .with_description("Allows grantee to create new objects in the bucket. For the bucket and object owners of existing objects, also allows deletions and overwrites of thos...")
                .with_provider_name("GrantWrite"),
        )
        .attribute(
            AttributeSchema::new("grant_write_acp", AttributeType::String)
                .with_description("Allows grantee to write the ACL for the applicable bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantWriteACP"),
        )
        .attribute(
            AttributeSchema::new("object_lock_enabled_for_bucket", AttributeType::Bool)
                .create_only()
                .with_description("Specifies whether you want S3 Object Lock to be enabled for the new bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("ObjectLockEnabledForBucket"),
        )
        .attribute(
            AttributeSchema::new("object_ownership", AttributeType::Custom {
                name: "ObjectOwnership".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_object_ownership,
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: None,
            })
                .with_provider_name("ObjectOwnership")
                .with_completions(vec![CompletionValue::new("aws.s3.bucket.ObjectOwnership.BucketOwnerEnforced", "BucketOwnerEnforced"), CompletionValue::new("aws.s3.bucket.ObjectOwnership.BucketOwnerPreferred", "BucketOwnerPreferred"), CompletionValue::new("aws.s3.bucket.ObjectOwnership.ObjectWriter", "ObjectWriter")]),
        )
        .attribute(
            AttributeSchema::new("versioning_status", AttributeType::Custom {
                name: "VersioningStatus".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_versioning_status,
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: None,
            })
                .with_description("The versioning state of the bucket.")
                .with_provider_name("VersioningStatus")
                .with_completions(vec![CompletionValue::new("aws.s3.bucket.VersioningStatus.Enabled", "Enabled"), CompletionValue::new("aws.s3.bucket.VersioningStatus.Suspended", "Suspended")]),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the resource.")
                .with_provider_name("Tags"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "s3.bucket",
        &[
            ("acl", VALID_ACL),
            ("object_ownership", VALID_OBJECT_OWNERSHIP),
            ("versioning_status", VALID_VERSIONING_STATUS),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
