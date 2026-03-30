//! bucket schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.s3
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

const VALID_ACL: &[&str] = &[
    "authenticated-read",
    "private",
    "public-read",
    "public-read-write",
];

const VALID_BUCKET_NAMESPACE: &[&str] = &["account-regional", "global"];

const VALID_OBJECT_OWNERSHIP: &[&str] = &[
    "BucketOwnerEnforced",
    "BucketOwnerPreferred",
    "ObjectWriter",
];

const VALID_VERSIONING_STATUS: &[&str] = &["Enabled", "Suspended"];

/// Returns the schema config for s3.bucket (Smithy: com.amazonaws.s3)
pub fn s3_bucket_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::S3::Bucket",
        resource_type_name: "s3.bucket",
        has_tags: true,
        schema: ResourceSchema::new("aws.s3.bucket")
        .attribute(
            AttributeSchema::new("acl", AttributeType::StringEnum {
                name: "ACL".to_string(),
                values: vec!["authenticated-read".to_string(), "private".to_string(), "public-read".to_string(), "public-read-write".to_string()],
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })
                .with_description("The canned ACL to apply to the bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("ACL"),
        )
        .attribute(
            AttributeSchema::new("bucket", AttributeType::String)
                .required()
                .create_only()
                .with_description("The name of the bucket to create. General purpose buckets - For information about bucket naming restrictions, see Bucket naming rules in the Amazon S3...")
                .with_provider_name("Bucket"),
        )
        .attribute(
            AttributeSchema::new("bucket_namespace", AttributeType::StringEnum {
                name: "BucketNamespace".to_string(),
                values: vec!["account-regional".to_string(), "global".to_string()],
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })
                .create_only()
                .with_description("Specifies the namespace where you want to create your general purpose bucket. When you create a general purpose bucket, you can choose to create a buc...")
                .with_provider_name("BucketNamespace"),
        )
        .attribute(
            AttributeSchema::new("grant_full_control", super::s3_grantee())
                .with_description("Allows grantee the read, write, read ACP, and write ACP permissions on the bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantFullControl"),
        )
        .attribute(
            AttributeSchema::new("grant_read", super::s3_grantee())
                .with_description("Allows grantee to list the objects in the bucket. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantRead"),
        )
        .attribute(
            AttributeSchema::new("grant_read_acp", super::s3_grantee())
                .with_description("Allows grantee to read the bucket ACL. This functionality is not supported for directory buckets.")
                .with_provider_name("GrantReadACP"),
        )
        .attribute(
            AttributeSchema::new("grant_write", super::s3_grantee())
                .with_description("Allows grantee to create new objects in the bucket. For the bucket and object owners of existing objects, also allows deletions and overwrites of thos...")
                .with_provider_name("GrantWrite"),
        )
        .attribute(
            AttributeSchema::new("grant_write_acp", super::s3_grantee())
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
            AttributeSchema::new("object_ownership", AttributeType::StringEnum {
                name: "ObjectOwnership".to_string(),
                values: vec!["BucketOwnerEnforced".to_string(), "BucketOwnerPreferred".to_string(), "ObjectWriter".to_string()],
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: None,
            })
                .with_provider_name("ObjectOwnership"),
        )
        .attribute(
            AttributeSchema::new("versioning_status", AttributeType::StringEnum {
                name: "VersioningStatus".to_string(),
                values: vec!["Enabled".to_string(), "Suspended".to_string()],
                namespace: Some("aws.s3.bucket".to_string()),
                to_dsl: None,
            })
                .with_description("The versioning state of the bucket.")
                .with_provider_name("VersioningStatus"),
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
            ("bucket_namespace", VALID_BUCKET_NAMESPACE),
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
