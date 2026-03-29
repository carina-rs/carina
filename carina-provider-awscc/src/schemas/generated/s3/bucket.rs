//! bucket schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::S3::Bucket
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
use regex::Regex;

const VALID_ABAC_STATUS: &[&str] = &["Enabled", "Disabled"];

const VALID_ACCELERATE_CONFIGURATION_ACCELERATION_STATUS: &[&str] = &["Enabled", "Suspended"];

const VALID_ACCESS_CONTROL: &[&str] = &[
    "AuthenticatedRead",
    "AwsExecRead",
    "BucketOwnerFullControl",
    "BucketOwnerRead",
    "LogDeliveryWrite",
    "Private",
    "PublicRead",
    "PublicReadWrite",
];

const VALID_ACCESS_CONTROL_TRANSLATION_OWNER: &[&str] = &["Destination"];

const VALID_BLOCKED_ENCRYPTION_TYPES_ENCRYPTION_TYPE: &[&str] = &["NONE", "SSE-C"];

const VALID_CORS_RULE_ALLOWED_METHODS: &[&str] = &["GET", "PUT", "HEAD", "POST", "DELETE"];

const VALID_DATA_EXPORT_OUTPUT_SCHEMA_VERSION: &[&str] = &["V_1"];

const VALID_DEFAULT_RETENTION_MODE: &[&str] = &["COMPLIANCE", "GOVERNANCE"];

const VALID_DELETE_MARKER_REPLICATION_STATUS: &[&str] = &["Disabled", "Enabled"];

const VALID_DESTINATION_FORMAT: &[&str] = &["CSV", "ORC", "Parquet"];

const VALID_INTELLIGENT_TIERING_CONFIGURATION_STATUS: &[&str] = &["Disabled", "Enabled"];

const VALID_INVENTORY_CONFIGURATION_INCLUDED_OBJECT_VERSIONS: &[&str] = &["All", "Current"];

const VALID_INVENTORY_CONFIGURATION_OPTIONAL_FIELDS: &[&str] = &[
    "Size",
    "LastModifiedDate",
    "StorageClass",
    "ETag",
    "IsMultipartUploaded",
    "ReplicationStatus",
    "EncryptionStatus",
    "ObjectLockRetainUntilDate",
    "ObjectLockMode",
    "ObjectLockLegalHoldStatus",
    "IntelligentTieringAccessTier",
    "BucketKeyStatus",
    "ChecksumAlgorithm",
    "ObjectAccessControlList",
    "ObjectOwner",
    "LifecycleExpirationDate",
];

const VALID_INVENTORY_CONFIGURATION_SCHEDULE_FREQUENCY: &[&str] = &["Daily", "Weekly"];

const VALID_INVENTORY_TABLE_CONFIGURATION_CONFIGURATION_STATE: &[&str] = &["ENABLED", "DISABLED"];

const VALID_LIFECYCLE_CONFIGURATION_TRANSITION_DEFAULT_MINIMUM_OBJECT_SIZE: &[&str] =
    &["varies_by_storage_class", "all_storage_classes_128K"];

const VALID_METADATA_DESTINATION_TABLE_BUCKET_TYPE: &[&str] = &["aws", "customer"];

const VALID_METADATA_TABLE_ENCRYPTION_CONFIGURATION_SSE_ALGORITHM: &[&str] = &["aws:kms", "AES256"];

const VALID_METRICS_STATUS: &[&str] = &["Disabled", "Enabled"];

const VALID_NONCURRENT_VERSION_TRANSITION_STORAGE_CLASS: &[&str] = &[
    "DEEP_ARCHIVE",
    "GLACIER",
    "GLACIER_IR",
    "INTELLIGENT_TIERING",
    "ONEZONE_IA",
    "STANDARD_IA",
];

const VALID_OBJECT_LOCK_CONFIGURATION_OBJECT_LOCK_ENABLED: &[&str] = &["Enabled"];

const VALID_OWNERSHIP_CONTROLS_RULE_OBJECT_OWNERSHIP: &[&str] = &[
    "ObjectWriter",
    "BucketOwnerPreferred",
    "BucketOwnerEnforced",
];

const VALID_PARTITIONED_PREFIX_PARTITION_DATE_SOURCE: &[&str] = &["EventTime", "DeliveryTime"];

const VALID_RECORD_EXPIRATION_EXPIRATION: &[&str] = &["ENABLED", "DISABLED"];

const VALID_REDIRECT_ALL_REQUESTS_TO_PROTOCOL: &[&str] = &["http", "https"];

const VALID_REDIRECT_RULE_PROTOCOL: &[&str] = &["http", "https"];

const VALID_REPLICA_MODIFICATIONS_STATUS: &[&str] = &["Enabled", "Disabled"];

const VALID_REPLICATION_DESTINATION_STORAGE_CLASS: &[&str] = &[
    "DEEP_ARCHIVE",
    "GLACIER",
    "GLACIER_IR",
    "INTELLIGENT_TIERING",
    "ONEZONE_IA",
    "REDUCED_REDUNDANCY",
    "STANDARD",
    "STANDARD_IA",
];

const VALID_REPLICATION_RULE_STATUS: &[&str] = &["Disabled", "Enabled"];

const VALID_REPLICATION_TIME_STATUS: &[&str] = &["Disabled", "Enabled"];

const VALID_RULE_STATUS: &[&str] = &["Enabled", "Disabled"];

const VALID_SERVER_SIDE_ENCRYPTION_BY_DEFAULT_SSE_ALGORITHM: &[&str] =
    &["aws:kms", "AES256", "aws:kms:dsse"];

const VALID_SSE_KMS_ENCRYPTED_OBJECTS_STATUS: &[&str] = &["Disabled", "Enabled"];

const VALID_TIERING_ACCESS_TIER: &[&str] = &["ARCHIVE_ACCESS", "DEEP_ARCHIVE_ACCESS"];

const VALID_TRANSITION_STORAGE_CLASS: &[&str] = &[
    "DEEP_ARCHIVE",
    "GLACIER",
    "GLACIER_IR",
    "INTELLIGENT_TIERING",
    "ONEZONE_IA",
    "STANDARD_IA",
];

const VALID_VERSIONING_CONFIGURATION_STATUS: &[&str] = &["Enabled", "Suspended"];

fn validate_days_after_initiation_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 0 {
            Err(format!("Value {} is out of range 0..", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

fn validate_max_age_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 0 {
            Err(format!("Value {} is out of range 0..", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

fn validate_string_pattern_cc806c69dc4cdaf7(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
            Regex::new("^(\\d{4})-(0[0-9]|1[0-2])-([0-2]\\d|3[01])T([01]\\d|2[0-4]):([0-5]\\d):([0-6]\\d)((\\.\\d{3})?)Z$").expect("invalid pattern regex")
        });
        if RE.is_match(s) {
            Ok(())
        } else {
            Err(format!(
                "Value '{}' does not match pattern ^(\\d{{4}})-(0[0-9]|1[0-2])-([0-2]\\d|3[01])T([01]\\d|2[0-4]):([0-5]\\d):([0-6]\\d)((\\.\\d{{3}})?)Z$",
                s
            ))
        }
    } else {
        Err("Expected string".to_string())
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

fn validate_string_length_max_1024(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        let len = s.chars().count();
        if len > 1024 {
            Err(format!("String length {} is out of range ..=1024", len))
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}

fn validate_string_pattern_3ee03875337c12ab_len_max_20(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        static RE: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new("[0-9]+").expect("invalid pattern regex"));
        if !RE.is_match(s) {
            return Err(format!("Value '{}' does not match pattern [0-9]+", s));
        }
        let len = s.chars().count();
        if len > 20 {
            return Err(format!("String length {} is out of range ..=20", len));
        }
        Ok(())
    } else {
        Err("Expected string".to_string())
    }
}

/// Returns the schema config for s3_bucket (AWS::S3::Bucket)
pub fn s3_bucket_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::S3::Bucket",
        resource_type_name: "s3.bucket",
        has_tags: true,
        schema: ResourceSchema::new("awscc.s3.bucket")
        .with_description("The ``AWS::S3::Bucket`` resource creates an Amazon S3 bucket in the same AWS Region where you create the AWS CloudFormation stack.  To control how AWS CloudFormation handles the bucket when the stack is deleted, you can set a deletion policy for your bucket. You can choose to *retain* the bucket or to *delete* the bucket. For more information, see [DeletionPolicy Attribute](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-attribute-deletionpolicy.html).   You can only delete empty buckets. Deletion fails for buckets that have contents.")
        .attribute(
            AttributeSchema::new("abac_status", AttributeType::StringEnum {
                name: "AbacStatus".to_string(),
                values: vec!["Enabled".to_string(), "Disabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            })
                .with_description("The ABAC status of the general purpose bucket. When ABAC is enabled for the general purpose bucket, you can use tags to manage access to the general purpose buckets as well as for cost tracking purposes. When ABAC is disabled for the general purpose buckets, you can only use tags for cost tracking purposes. For more information, see [Using tags with S3 general purpose buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/buckets-tagging.html).")
                .with_provider_name("AbacStatus"),
        )
        .attribute(
            AttributeSchema::new("accelerate_configuration", AttributeType::Struct {
                    name: "AccelerateConfiguration".to_string(),
                    fields: vec![
                    StructField::new("acceleration_status", AttributeType::StringEnum {
                name: "AccelerationStatus".to_string(),
                values: vec!["Enabled".to_string(), "Suspended".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies the transfer acceleration status of the bucket.").with_provider_name("AccelerationStatus")
                    ],
                })
                .with_description("Configures the transfer acceleration state for an Amazon S3 bucket. For more information, see [Amazon S3 Transfer Acceleration](https://docs.aws.amazon.com/AmazonS3/latest/dev/transfer-acceleration.html) in the *Amazon S3 User Guide*.")
                .with_provider_name("AccelerateConfiguration"),
        )
        .attribute(
            AttributeSchema::new("access_control", AttributeType::StringEnum {
                name: "AccessControl".to_string(),
                values: vec!["AuthenticatedRead".to_string(), "AwsExecRead".to_string(), "BucketOwnerFullControl".to_string(), "BucketOwnerRead".to_string(), "LogDeliveryWrite".to_string(), "Private".to_string(), "PublicRead".to_string(), "PublicReadWrite".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            })
                .write_only()
                .with_description("This is a legacy property, and it is not recommended for most use cases. A majority of modern use cases in Amazon S3 no longer require the use of ACLs, and we recommend that you keep ACLs disabled. For more information, see [Controlling object ownership](https://docs.aws.amazon.com//AmazonS3/latest/userguide/about-object-ownership.html) in the *Amazon S3 User Guide*. A canned access control list (ACL) that grants predefined permissions to the bucket. For more information about canned ACLs, see [Canned ACL](https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#canned-acl) in the *Amazon S3 User Guide*. S3 buckets are created with ACLs disabled by default. Therefore, unless you explicitly set the [AWS::S3::OwnershipControls](https://docs.aws.amazon.com//AWSCloudFormation/latest/UserGuide/aws-properties-s3-bucket-ownershipcontrols.html) property to enable ACLs, your resource will fail to deploy with any value other than Private. Use cases requiring ACLs are uncommon. The majority of access control configurations can be successfully and more easily achieved with bucket policies. For more information, see [AWS::S3::BucketPolicy](https://docs.aws.amazon.com//AWSCloudFormation/latest/UserGuide/aws-properties-s3-policy.html). For examples of common policy configurations, including S3 Server Access Logs buckets and more, see [Bucket policy examples](https://docs.aws.amazon.com/AmazonS3/latest/userguide/example-bucket-policies.html) in the *Amazon S3 User Guide*.")
                .with_provider_name("AccessControl"),
        )
        .attribute(
            AttributeSchema::new("analytics_configurations", AttributeType::list(AttributeType::Struct {
                    name: "AnalyticsConfiguration".to_string(),
                    fields: vec![
                    StructField::new("id", AttributeType::String).required().with_description("The ID that identifies the analytics configuration.").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix that an object must have to be included in the analytics results.").with_provider_name("Prefix"),
                    StructField::new("storage_class_analysis", AttributeType::Struct {
                    name: "StorageClassAnalysis".to_string(),
                    fields: vec![
                    StructField::new("data_export", AttributeType::Struct {
                    name: "DataExport".to_string(),
                    fields: vec![
                    StructField::new("destination", AttributeType::Struct {
                    name: "Destination".to_string(),
                    fields: vec![
                    StructField::new("bucket_account_id", super::aws_account_id()).with_description("The account ID that owns the destination S3 bucket. If no account ID is provided, the owner is not validated before exporting data. Although this value is optional, we strongly recommend that you set it to help prevent problems if the destination bucket ownership changes.").with_provider_name("BucketAccountId"),
                    StructField::new("bucket_arn", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the bucket to which data is exported.").with_provider_name("BucketArn"),
                    StructField::new("format", AttributeType::StringEnum {
                name: "Format".to_string(),
                values: vec!["CSV".to_string(), "ORC".to_string(), "Parquet".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies the file format used when exporting data to Amazon S3. *Allowed values*: ``CSV`` | ``ORC`` | ``Parquet``").with_provider_name("Format"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix to use when exporting data. The prefix is prepended to all results.").with_provider_name("Prefix")
                    ],
                }).required().with_description("The place to store the data for an analysis.").with_provider_name("Destination"),
                    StructField::new("output_schema_version", AttributeType::StringEnum {
                name: "OutputSchemaVersion".to_string(),
                values: vec!["V_1".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The version of the output schema to use when exporting data. Must be ``V_1``.").with_provider_name("OutputSchemaVersion")
                    ],
                }).with_description("Specifies how data related to the storage class analysis for an Amazon S3 bucket should be exported.").with_provider_name("DataExport")
                    ],
                }).required().with_description("Contains data related to access patterns to be collected and made available to analyze the tradeoffs between different storage classes.").with_provider_name("StorageClassAnalysis"),
                    StructField::new("tag_filters", AttributeType::list(tags_type())).with_description("The tags to use when evaluating an analytics filter. The analytics only includes objects that meet the filter's criteria. If no filter is specified, all of the contents of the bucket are included in the analysis.").with_provider_name("TagFilters")
                    ],
                }))
                .with_description("Specifies the configuration and any analyses for the analytics filter of an Amazon S3 bucket.")
                .with_provider_name("AnalyticsConfigurations")
                .with_block_name("analytics_configuration"),
        )
        .attribute(
            AttributeSchema::new("arn", super::arn())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("bucket_encryption", AttributeType::Struct {
                    name: "BucketEncryption".to_string(),
                    fields: vec![
                    StructField::new("server_side_encryption_configuration", AttributeType::list(AttributeType::Struct {
                    name: "ServerSideEncryptionRule".to_string(),
                    fields: vec![
                    StructField::new("blocked_encryption_types", AttributeType::Struct {
                    name: "BlockedEncryptionTypes".to_string(),
                    fields: vec![
                    StructField::new("encryption_type", AttributeType::list(AttributeType::StringEnum {
                name: "EncryptionType".to_string(),
                values: vec!["NONE".to_string(), "SSE-C".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })).with_description("The object encryption type that you want to block or unblock for an Amazon S3 general purpose bucket. Currently, this parameter only supports blocking or unblocking server side encryption with customer-provided keys (SSE-C). For more information about SSE-C, see [Using server-side encryption with customer-provided keys (SSE-C)](https://docs.aws.amazon.com/AmazonS3/latest/userguide/ServerSideEncryptionCustomerKeys.html).").with_provider_name("EncryptionType")
                    ],
                }).with_description("A bucket-level setting for Amazon S3 general purpose buckets used to prevent the upload of new objects encrypted with the specified server-side encryption type. For example, blocking an encryption type will block ``PutObject``, ``CopyObject``, ``PostObject``, multipart upload, and replication requests to the bucket for objects with the specified encryption type. However, you can continue to read and list any pre-existing objects already encrypted with the specified encryption type. For more information, see [Blocking or unblocking SSE-C for a general purpose bucket](https://docs.aws.amazon.com/AmazonS3/latest/userguide/blocking-unblocking-s3-c-encryption-gpb.html). Currently, this parameter only supports blocking or unblocking server-side encryption with customer-provided keys (SSE-C). For more information about SSE-C, see [Using server-side encryption with customer-provided keys (SSE-C)](https://docs.aws.amazon.com/AmazonS3/latest/userguide/ServerSideEncryptionCustomerKeys.html).").with_provider_name("BlockedEncryptionTypes"),
                    StructField::new("bucket_key_enabled", AttributeType::Bool).with_description("Specifies whether Amazon S3 should use an S3 Bucket Key with server-side encryption using KMS (SSE-KMS) for new objects in the bucket. Existing objects are not affected. Setting the ``BucketKeyEnabled`` element to ``true`` causes Amazon S3 to use an S3 Bucket Key. By default, S3 Bucket Key is not enabled. For more information, see [Amazon S3 Bucket Keys](https://docs.aws.amazon.com/AmazonS3/latest/dev/bucket-key.html) in the *Amazon S3 User Guide*.").with_provider_name("BucketKeyEnabled"),
                    StructField::new("server_side_encryption_by_default", AttributeType::Struct {
                    name: "ServerSideEncryptionByDefault".to_string(),
                    fields: vec![
                    StructField::new("kms_master_key_id", super::kms_key_id()).with_description("AWS Key Management Service (KMS) customer managed key ID to use for the default encryption. + *General purpose buckets* - This parameter is allowed if and only if ``SSEAlgorithm`` is set to ``aws:kms`` or ``aws:kms:dsse``. + *Directory buckets* - This parameter is allowed if and only if ``SSEAlgorithm`` is set to ``aws:kms``. You can specify the key ID, key alias, or the Amazon Resource Name (ARN) of the KMS key. + Key ID: ``1234abcd-12ab-34cd-56ef-1234567890ab`` + Key ARN: ``arn:aws:kms:us-east-2:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab`` + Key Alias: ``alias/alias-name`` If you are using encryption with cross-account or AWS service operations, you must use a fully qualified KMS key ARN. For more information, see [Using encryption for cross-account operations](https://docs.aws.amazon.com/AmazonS3/latest/dev/bucket-encryption.html#bucket-encryption-update-bucket-policy). + *General purpose buckets* - If you're specifying a customer managed KMS key, we recommend using a fully qualified KMS key ARN. If you use a KMS key alias instead, then KMS resolves the key within the requester?s account. This behavior can result in data that's encrypted with a KMS key that belongs to the requester, and not the bucket owner. Also, if you use a key ID, you can run into a LogDestination undeliverable error when creating a VPC flow log. + *Directory buckets* - When you specify an [customer managed key](https://docs.aws.amazon.com/kms/latest/developerguide/concepts.html#customer-cmk) for encryption in your directory bucket, only use the key ID or key ARN. The key alias format of the KMS key isn't supported. Amazon S3 only supports symmetric encryption KMS keys. For more information, see [Asymmetric keys in KMS](https://docs.aws.amazon.com//kms/latest/developerguide/symmetric-asymmetric.html) in the *Key Management Service Developer Guide*.").with_provider_name("KMSMasterKeyID"),
                    StructField::new("sse_algorithm", AttributeType::StringEnum {
                name: "ServerSideEncryptionByDefaultSseAlgorithm".to_string(),
                values: vec!["aws:kms".to_string(), "AES256".to_string(), "aws:kms:dsse".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Server-side encryption algorithm to use for the default encryption. For directory buckets, there are only two supported values for server-side encryption: ``AES256`` and ``aws:kms``.").with_provider_name("SSEAlgorithm")
                    ],
                }).with_description("Specifies the default server-side encryption to apply to new objects in the bucket. If a PUT Object request doesn't specify any server-side encryption, this default encryption will be applied.").with_provider_name("ServerSideEncryptionByDefault")
                    ],
                })).required().with_description("Specifies the default server-side-encryption configuration.").with_provider_name("ServerSideEncryptionConfiguration").with_block_name("server_side_encryption_configuration")
                    ],
                })
                .with_description("Specifies default encryption for a bucket using server-side encryption with Amazon S3-managed keys (SSE-S3), AWS KMS-managed keys (SSE-KMS), or dual-layer server-side encryption with KMS-managed keys (DSSE-KMS). For information about the Amazon S3 default encryption feature, see [Amazon S3 Default Encryption for S3 Buckets](https://docs.aws.amazon.com/AmazonS3/latest/dev/bucket-encryption.html) in the *Amazon S3 User Guide*.")
                .with_provider_name("BucketEncryption")
                .with_block_name("bucket_encryption"),
        )
        .attribute(
            AttributeSchema::new("bucket_name", AttributeType::String)
                .create_only()
                .with_description("A name for the bucket. If you don't specify a name, AWS CloudFormation generates a unique ID and uses that ID for the bucket name. The bucket name must contain only lowercase letters, numbers, periods (.), and dashes (-) and must follow [Amazon S3 bucket restrictions and limitations](https://docs.aws.amazon.com/AmazonS3/latest/dev/BucketRestrictions.html). For more information, see [Rules for naming Amazon S3 buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html) in the *Amazon S3 User Guide*. If you specify a name, you can't perform updates that require replacement of this resource. You can perform updates that require no or some interruption. If you need to replace the resource, specify a new name.")
                .with_provider_name("BucketName"),
        )
        .attribute(
            AttributeSchema::new("cors_configuration", AttributeType::Struct {
                    name: "CorsConfiguration".to_string(),
                    fields: vec![
                    StructField::new("cors_rules", AttributeType::list(AttributeType::Struct {
                    name: "CorsRule".to_string(),
                    fields: vec![
                    StructField::new("allowed_headers", AttributeType::list(AttributeType::String)).with_description("Headers that are specified in the ``Access-Control-Request-Headers`` header. These headers are allowed in a preflight OPTIONS request. In response to any preflight OPTIONS request, Amazon S3 returns any requested headers that are allowed.").with_provider_name("AllowedHeaders"),
                    StructField::new("allowed_methods", AttributeType::list(AttributeType::StringEnum {
                name: "AllowedMethods".to_string(),
                values: vec!["GET".to_string(), "PUT".to_string(), "HEAD".to_string(), "POST".to_string(), "DELETE".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            })).required().with_description("An HTTP method that you allow the origin to run. *Allowed values*: ``GET`` | ``PUT`` | ``HEAD`` | ``POST`` | ``DELETE``").with_provider_name("AllowedMethods"),
                    StructField::new("allowed_origins", AttributeType::list(AttributeType::String)).required().with_description("One or more origins you want customers to be able to access the bucket from.").with_provider_name("AllowedOrigins"),
                    StructField::new("exposed_headers", AttributeType::list(AttributeType::String)).with_description("One or more headers in the response that you want customers to be able to access from their applications (for example, from a JavaScript ``XMLHttpRequest`` object).").with_provider_name("ExposedHeaders"),
                    StructField::new("id", AttributeType::Custom {
                name: "String(len: ..=255)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_255,
                namespace: None,
                to_dsl: None,
            }).with_description("A unique identifier for this rule. The value must be no more than 255 characters.").with_provider_name("Id"),
                    StructField::new("max_age", AttributeType::Custom {
                name: "Int(0..)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_max_age_range,
                namespace: None,
                to_dsl: None,
            }).with_description("The time in seconds that your browser is to cache the preflight response for the specified resource.").with_provider_name("MaxAge")
                    ],
                })).required().with_description("A set of origins and methods (cross-origin access that you want to allow). You can add up to 100 rules to the configuration.").with_provider_name("CorsRules").with_block_name("cors_rule")
                    ],
                })
                .with_description("Describes the cross-origin access configuration for objects in an Amazon S3 bucket. For more information, see [Enabling Cross-Origin Resource Sharing](https://docs.aws.amazon.com/AmazonS3/latest/dev/cors.html) in the *Amazon S3 User Guide*.")
                .with_provider_name("CorsConfiguration")
                .with_block_name("cors_configuration"),
        )
        .attribute(
            AttributeSchema::new("domain_name", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("DomainName"),
        )
        .attribute(
            AttributeSchema::new("dual_stack_domain_name", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("DualStackDomainName"),
        )
        .attribute(
            AttributeSchema::new("intelligent_tiering_configurations", AttributeType::list(AttributeType::Struct {
                    name: "IntelligentTieringConfiguration".to_string(),
                    fields: vec![
                    StructField::new("id", AttributeType::String).required().with_description("The ID used to identify the S3 Intelligent-Tiering configuration.").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the subset of objects to which the rule applies.").with_provider_name("Prefix"),
                    StructField::new("status", AttributeType::StringEnum {
                name: "IntelligentTieringConfigurationStatus".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies the status of the configuration.").with_provider_name("Status"),
                    StructField::new("tag_filters", AttributeType::list(tags_type())).with_description("A container for a key-value pair.").with_provider_name("TagFilters"),
                    StructField::new("tierings", AttributeType::list(AttributeType::Struct {
                    name: "Tiering".to_string(),
                    fields: vec![
                    StructField::new("access_tier", AttributeType::StringEnum {
                name: "AccessTier".to_string(),
                values: vec!["ARCHIVE_ACCESS".to_string(), "DEEP_ARCHIVE_ACCESS".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("S3 Intelligent-Tiering access tier. See [Storage class for automatically optimizing frequently and infrequently accessed objects](https://docs.aws.amazon.com/AmazonS3/latest/dev/storage-class-intro.html#sc-dynamic-data-access) for a list of access tiers in the S3 Intelligent-Tiering storage class.").with_provider_name("AccessTier"),
                    StructField::new("days", AttributeType::Int).required().with_description("The number of consecutive days of no access after which an object will be eligible to be transitioned to the corresponding tier. The minimum number of days specified for Archive Access tier must be at least 90 days and Deep Archive Access tier must be at least 180 days. The maximum can be up to 2 years (730 days).").with_provider_name("Days")
                    ],
                })).required().with_description("Specifies a list of S3 Intelligent-Tiering storage class tiers in the configuration. At least one tier must be defined in the list. At most, you can specify two tiers in the list, one for each available AccessTier: ``ARCHIVE_ACCESS`` and ``DEEP_ARCHIVE_ACCESS``. You only need Intelligent Tiering Configuration enabled on a bucket if you want to automatically move objects stored in the Intelligent-Tiering storage class to Archive Access or Deep Archive Access tiers.").with_provider_name("Tierings").with_block_name("tiering")
                    ],
                }))
                .with_description("Defines how Amazon S3 handles Intelligent-Tiering storage.")
                .with_provider_name("IntelligentTieringConfigurations")
                .with_block_name("intelligent_tiering_configuration"),
        )
        .attribute(
            AttributeSchema::new("inventory_configurations", AttributeType::list(AttributeType::Struct {
                    name: "InventoryConfiguration".to_string(),
                    fields: vec![
                    StructField::new("destination", AttributeType::Struct {
                    name: "Destination".to_string(),
                    fields: vec![
                    StructField::new("bucket_account_id", super::aws_account_id()).with_description("The account ID that owns the destination S3 bucket. If no account ID is provided, the owner is not validated before exporting data. Although this value is optional, we strongly recommend that you set it to help prevent problems if the destination bucket ownership changes.").with_provider_name("BucketAccountId"),
                    StructField::new("bucket_arn", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the bucket to which data is exported.").with_provider_name("BucketArn"),
                    StructField::new("format", AttributeType::StringEnum {
                name: "Format".to_string(),
                values: vec!["CSV".to_string(), "ORC".to_string(), "Parquet".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies the file format used when exporting data to Amazon S3. *Allowed values*: ``CSV`` | ``ORC`` | ``Parquet``").with_provider_name("Format"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix to use when exporting data. The prefix is prepended to all results.").with_provider_name("Prefix")
                    ],
                }).required().with_description("Contains information about where to publish the inventory results.").with_provider_name("Destination"),
                    StructField::new("enabled", AttributeType::Bool).required().with_description("Specifies whether the inventory is enabled or disabled. If set to ``True``, an inventory list is generated. If set to ``False``, no inventory list is generated.").with_provider_name("Enabled"),
                    StructField::new("id", AttributeType::String).required().with_description("The ID used to identify the inventory configuration.").with_provider_name("Id"),
                    StructField::new("included_object_versions", AttributeType::StringEnum {
                name: "IncludedObjectVersions".to_string(),
                values: vec!["All".to_string(), "Current".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Object versions to include in the inventory list. If set to ``All``, the list includes all the object versions, which adds the version-related fields ``VersionId``, ``IsLatest``, and ``DeleteMarker`` to the list. If set to ``Current``, the list does not contain these version-related fields.").with_provider_name("IncludedObjectVersions"),
                    StructField::new("optional_fields", AttributeType::list(AttributeType::StringEnum {
                name: "OptionalFields".to_string(),
                values: vec!["Size".to_string(), "LastModifiedDate".to_string(), "StorageClass".to_string(), "ETag".to_string(), "IsMultipartUploaded".to_string(), "ReplicationStatus".to_string(), "EncryptionStatus".to_string(), "ObjectLockRetainUntilDate".to_string(), "ObjectLockMode".to_string(), "ObjectLockLegalHoldStatus".to_string(), "IntelligentTieringAccessTier".to_string(), "BucketKeyStatus".to_string(), "ChecksumAlgorithm".to_string(), "ObjectAccessControlList".to_string(), "ObjectOwner".to_string(), "LifecycleExpirationDate".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            })).with_description("Contains the optional fields that are included in the inventory results.").with_provider_name("OptionalFields"),
                    StructField::new("prefix", AttributeType::String).with_description("Specifies the inventory filter prefix.").with_provider_name("Prefix"),
                    StructField::new("schedule_frequency", AttributeType::StringEnum {
                name: "ScheduleFrequency".to_string(),
                values: vec!["Daily".to_string(), "Weekly".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies the schedule for generating inventory results.").with_provider_name("ScheduleFrequency")
                    ],
                }))
                .with_description("Specifies the S3 Inventory configuration for an Amazon S3 bucket. For more information, see [GET Bucket inventory](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucketGETInventoryConfig.html) in the *Amazon S3 API Reference*.")
                .with_provider_name("InventoryConfigurations")
                .with_block_name("inventory_configuration"),
        )
        .attribute(
            AttributeSchema::new("lifecycle_configuration", AttributeType::Struct {
                    name: "LifecycleConfiguration".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::list(AttributeType::Struct {
                    name: "Rule".to_string(),
                    fields: vec![
                    StructField::new("abort_incomplete_multipart_upload", AttributeType::Struct {
                    name: "AbortIncompleteMultipartUpload".to_string(),
                    fields: vec![
                    StructField::new("days_after_initiation", AttributeType::Custom {
                name: "Int(0..)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_days_after_initiation_range,
                namespace: None,
                to_dsl: None,
            }).required().with_description("Specifies the number of days after which Amazon S3 stops an incomplete multipart upload.").with_provider_name("DaysAfterInitiation")
                    ],
                }).with_description("Specifies a lifecycle rule that stops incomplete multipart uploads to an Amazon S3 bucket.").with_provider_name("AbortIncompleteMultipartUpload"),
                    StructField::new("expiration_date", AttributeType::Custom {
                name: "String(pattern)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_pattern_cc806c69dc4cdaf7,
                namespace: None,
                to_dsl: None,
            }).with_description("Indicates when objects are deleted from Amazon S3 and Amazon S3 Glacier. The date value must be in ISO 8601 format. The time is always midnight UTC. If you specify an expiration and transition time, you must use the same time unit for both properties (either in days or by date). The expiration time must also be later than the transition time.").with_provider_name("ExpirationDate"),
                    StructField::new("expiration_in_days", AttributeType::Int).with_description("Indicates the number of days after creation when objects are deleted from Amazon S3 and Amazon S3 Glacier. If you specify an expiration and transition time, you must use the same time unit for both properties (either in days or by date). The expiration time must also be later than the transition time.").with_provider_name("ExpirationInDays"),
                    StructField::new("expired_object_delete_marker", AttributeType::Bool).with_description("Indicates whether Amazon S3 will remove a delete marker without any noncurrent versions. If set to true, the delete marker will be removed if there are no noncurrent versions. This cannot be specified with ``ExpirationInDays``, ``ExpirationDate``, or ``TagFilters``.").with_provider_name("ExpiredObjectDeleteMarker"),
                    StructField::new("id", AttributeType::Custom {
                name: "String(len: ..=255)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_255,
                namespace: None,
                to_dsl: None,
            }).with_description("Unique identifier for the rule. The value can't be longer than 255 characters.").with_provider_name("Id"),
                    StructField::new("noncurrent_version_expiration", AttributeType::Struct {
                    name: "NoncurrentVersionExpiration".to_string(),
                    fields: vec![
                    StructField::new("newer_noncurrent_versions", AttributeType::Int).with_description("Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent versions, S3 will take the associated action. For more information about noncurrent versions, see [Lifecycle configuration elements](https://docs.aws.amazon.com/AmazonS3/latest/userguide/intro-lifecycle-rules.html) in the *Amazon S3 User Guide*.").with_provider_name("NewerNoncurrentVersions"),
                    StructField::new("noncurrent_days", AttributeType::Int).required().with_description("Specifies the number of days an object is noncurrent before S3 can perform the associated action. For information about the noncurrent days calculations, see [How Amazon S3 Calculates When an Object Became Noncurrent](https://docs.aws.amazon.com/AmazonS3/latest/dev/intro-lifecycle-rules.html#non-current-days-calculations) in the *Amazon S3 User Guide*.").with_provider_name("NoncurrentDays")
                    ],
                }).with_description("Specifies when noncurrent object versions expire. Upon expiration, S3 permanently deletes the noncurrent object versions. You set this lifecycle configuration action on a bucket that has versioning enabled (or suspended) to request that S3 delete noncurrent object versions at a specific period in the object's lifetime.").with_provider_name("NoncurrentVersionExpiration"),
                    StructField::new("noncurrent_version_expiration_in_days", AttributeType::Int).with_description("(Deprecated.) For buckets with versioning enabled (or suspended), specifies the time, in days, between when a new version of the object is uploaded to the bucket and when old versions of the object expire. When object versions expire, Amazon S3 permanently deletes them. If you specify a transition and expiration time, the expiration time must be later than the transition time.").with_provider_name("NoncurrentVersionExpirationInDays"),
                    StructField::new("noncurrent_version_transition", AttributeType::Struct {
                    name: "NoncurrentVersionTransition".to_string(),
                    fields: vec![
                    StructField::new("newer_noncurrent_versions", AttributeType::Int).with_description("Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent versions, S3 will take the associated action. For more information about noncurrent versions, see [Lifecycle configuration elements](https://docs.aws.amazon.com/AmazonS3/latest/userguide/intro-lifecycle-rules.html) in the *Amazon S3 User Guide*.").with_provider_name("NewerNoncurrentVersions"),
                    StructField::new("storage_class", AttributeType::StringEnum {
                name: "NoncurrentVersionTransitionStorageClass".to_string(),
                values: vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The class of storage used to store the object.").with_provider_name("StorageClass"),
                    StructField::new("transition_in_days", AttributeType::Int).required().with_description("Specifies the number of days an object is noncurrent before Amazon S3 can perform the associated action. For information about the noncurrent days calculations, see [How Amazon S3 Calculates How Long an Object Has Been Noncurrent](https://docs.aws.amazon.com/AmazonS3/latest/dev/intro-lifecycle-rules.html#non-current-days-calculations) in the *Amazon S3 User Guide*.").with_provider_name("TransitionInDays")
                    ],
                }).with_description("(Deprecated.) For buckets with versioning enabled (or suspended), specifies when non-current objects transition to a specified storage class. If you specify a transition and expiration time, the expiration time must be later than the transition time. If you specify this property, don't specify the ``NoncurrentVersionTransitions`` property.").with_provider_name("NoncurrentVersionTransition"),
                    StructField::new("noncurrent_version_transitions", AttributeType::list(AttributeType::Struct {
                    name: "NoncurrentVersionTransition".to_string(),
                    fields: vec![
                    StructField::new("newer_noncurrent_versions", AttributeType::Int).with_description("Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent versions, S3 will take the associated action. For more information about noncurrent versions, see [Lifecycle configuration elements](https://docs.aws.amazon.com/AmazonS3/latest/userguide/intro-lifecycle-rules.html) in the *Amazon S3 User Guide*.").with_provider_name("NewerNoncurrentVersions"),
                    StructField::new("storage_class", AttributeType::StringEnum {
                name: "NoncurrentVersionTransitionStorageClass".to_string(),
                values: vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The class of storage used to store the object.").with_provider_name("StorageClass"),
                    StructField::new("transition_in_days", AttributeType::Int).required().with_description("Specifies the number of days an object is noncurrent before Amazon S3 can perform the associated action. For information about the noncurrent days calculations, see [How Amazon S3 Calculates How Long an Object Has Been Noncurrent](https://docs.aws.amazon.com/AmazonS3/latest/dev/intro-lifecycle-rules.html#non-current-days-calculations) in the *Amazon S3 User Guide*.").with_provider_name("TransitionInDays")
                    ],
                })).with_description("For buckets with versioning enabled (or suspended), one or more transition rules that specify when non-current objects transition to a specified storage class. If you specify a transition and expiration time, the expiration time must be later than the transition time. If you specify this property, don't specify the ``NoncurrentVersionTransition`` property.").with_provider_name("NoncurrentVersionTransitions").with_block_name("noncurrent_version_transition"),
                    StructField::new("object_size_greater_than", AttributeType::Custom {
                name: "NumericString(len: ..=20)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_pattern_3ee03875337c12ab_len_max_20,
                namespace: None,
                to_dsl: None,
            }).with_description("Specifies the minimum object size in bytes for this rule to apply to. Objects must be larger than this value in bytes. For more information about size based rules, see [Lifecycle configuration using size-based rules](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-configuration-examples.html#lc-size-rules) in the *Amazon S3 User Guide*.").with_provider_name("ObjectSizeGreaterThan"),
                    StructField::new("object_size_less_than", AttributeType::Custom {
                name: "NumericString(len: ..=20)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_pattern_3ee03875337c12ab_len_max_20,
                namespace: None,
                to_dsl: None,
            }).with_description("Specifies the maximum object size in bytes for this rule to apply to. Objects must be smaller than this value in bytes. For more information about sized based rules, see [Lifecycle configuration using size-based rules](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-configuration-examples.html#lc-size-rules) in the *Amazon S3 User Guide*.").with_provider_name("ObjectSizeLessThan"),
                    StructField::new("prefix", AttributeType::String).with_description("Object key prefix that identifies one or more objects to which this rule applies. Replacement must be made for object keys containing special characters (such as carriage returns) when using XML requests. For more information, see [XML related object key constraints](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html#object-key-xml-related-constraints).").with_provider_name("Prefix"),
                    StructField::new("status", AttributeType::StringEnum {
                name: "RuleStatus".to_string(),
                values: vec!["Enabled".to_string(), "Disabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("If ``Enabled``, the rule is currently being applied. If ``Disabled``, the rule is not currently being applied.").with_provider_name("Status"),
                    StructField::new("tag_filters", AttributeType::list(tags_type())).with_description("Tags to use to identify a subset of objects to which the lifecycle rule applies.").with_provider_name("TagFilters"),
                    StructField::new("transition", AttributeType::Struct {
                    name: "Transition".to_string(),
                    fields: vec![
                    StructField::new("storage_class", AttributeType::StringEnum {
                name: "TransitionStorageClass".to_string(),
                values: vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The storage class to which you want the object to transition.").with_provider_name("StorageClass"),
                    StructField::new("transition_date", AttributeType::Custom {
                name: "String(pattern)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_pattern_cc806c69dc4cdaf7,
                namespace: None,
                to_dsl: None,
            }).with_description("Indicates when objects are transitioned to the specified storage class. The date value must be in ISO 8601 format. The time is always midnight UTC.").with_provider_name("TransitionDate"),
                    StructField::new("transition_in_days", AttributeType::Int).with_description("Indicates the number of days after creation when objects are transitioned to the specified storage class. If the specified storage class is ``INTELLIGENT_TIERING``, ``GLACIER_IR``, ``GLACIER``, or ``DEEP_ARCHIVE``, valid values are ``0`` or positive integers. If the specified storage class is ``STANDARD_IA`` or ``ONEZONE_IA``, valid values are positive integers greater than ``30``. Be aware that some storage classes have a minimum storage duration and that you're charged for transitioning objects before their minimum storage duration. For more information, see [Constraints and considerations for transitions](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-transition-general-considerations.html#lifecycle-configuration-constraints) in the *Amazon S3 User Guide*.").with_provider_name("TransitionInDays")
                    ],
                }).with_description("(Deprecated.) Specifies when an object transitions to a specified storage class. If you specify an expiration and transition time, you must use the same time unit for both properties (either in days or by date). The expiration time must also be later than the transition time. If you specify this property, don't specify the ``Transitions`` property.").with_provider_name("Transition"),
                    StructField::new("transitions", AttributeType::list(AttributeType::Struct {
                    name: "Transition".to_string(),
                    fields: vec![
                    StructField::new("storage_class", AttributeType::StringEnum {
                name: "TransitionStorageClass".to_string(),
                values: vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The storage class to which you want the object to transition.").with_provider_name("StorageClass"),
                    StructField::new("transition_date", AttributeType::Custom {
                name: "String(pattern)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_pattern_cc806c69dc4cdaf7,
                namespace: None,
                to_dsl: None,
            }).with_description("Indicates when objects are transitioned to the specified storage class. The date value must be in ISO 8601 format. The time is always midnight UTC.").with_provider_name("TransitionDate"),
                    StructField::new("transition_in_days", AttributeType::Int).with_description("Indicates the number of days after creation when objects are transitioned to the specified storage class. If the specified storage class is ``INTELLIGENT_TIERING``, ``GLACIER_IR``, ``GLACIER``, or ``DEEP_ARCHIVE``, valid values are ``0`` or positive integers. If the specified storage class is ``STANDARD_IA`` or ``ONEZONE_IA``, valid values are positive integers greater than ``30``. Be aware that some storage classes have a minimum storage duration and that you're charged for transitioning objects before their minimum storage duration. For more information, see [Constraints and considerations for transitions](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-transition-general-considerations.html#lifecycle-configuration-constraints) in the *Amazon S3 User Guide*.").with_provider_name("TransitionInDays")
                    ],
                })).with_description("One or more transition rules that specify when an object transitions to a specified storage class. If you specify an expiration and transition time, you must use the same time unit for both properties (either in days or by date). The expiration time must also be later than the transition time. If you specify this property, don't specify the ``Transition`` property.").with_provider_name("Transitions").with_block_name("transition")
                    ],
                })).required().with_description("A lifecycle rule for individual objects in an Amazon S3 bucket.").with_provider_name("Rules").with_block_name("rule"),
                    StructField::new("transition_default_minimum_object_size", AttributeType::StringEnum {
                name: "TransitionDefaultMinimumObjectSize".to_string(),
                values: vec!["varies_by_storage_class".to_string(), "all_storage_classes_128K".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Indicates which default minimum object size behavior is applied to the lifecycle configuration. This parameter applies to general purpose buckets only. It isn't supported for directory bucket lifecycle configurations. + ``all_storage_classes_128K`` - Objects smaller than 128 KB will not transition to any storage class by default. + ``varies_by_storage_class`` - Objects smaller than 128 KB will transition to Glacier Flexible Retrieval or Glacier Deep Archive storage classes. By default, all other storage classes will prevent transitions smaller than 128 KB. To customize the minimum object size for any transition you can add a filter that specifies a custom ``ObjectSizeGreaterThan`` or ``ObjectSizeLessThan`` in the body of your transition rule. Custom filters always take precedence over the default transition behavior.").with_provider_name("TransitionDefaultMinimumObjectSize")
                    ],
                })
                .with_description("Specifies the lifecycle configuration for objects in an Amazon S3 bucket. For more information, see [Object Lifecycle Management](https://docs.aws.amazon.com/AmazonS3/latest/dev/object-lifecycle-mgmt.html) in the *Amazon S3 User Guide*.")
                .with_provider_name("LifecycleConfiguration")
                .with_block_name("lifecycle_configuration"),
        )
        .attribute(
            AttributeSchema::new("logging_configuration", AttributeType::Struct {
                    name: "LoggingConfiguration".to_string(),
                    fields: vec![
                    StructField::new("destination_bucket_name", AttributeType::String).with_description("The name of the bucket where Amazon S3 should store server access log files. You can store log files in any bucket that you own. By default, logs are stored in the bucket where the ``LoggingConfiguration`` property is defined.").with_provider_name("DestinationBucketName"),
                    StructField::new("log_file_prefix", AttributeType::String).with_description("A prefix for all log object keys. If you store log files from multiple Amazon S3 buckets in a single bucket, you can use a prefix to distinguish which log files came from which bucket.").with_provider_name("LogFilePrefix"),
                    StructField::new("target_object_key_format", AttributeType::Struct {
                    name: "TargetObjectKeyFormat".to_string(),
                    fields: vec![
                    StructField::new("partitioned_prefix", AttributeType::Struct {
                    name: "PartitionedPrefix".to_string(),
                    fields: vec![
                    StructField::new("partition_date_source", AttributeType::StringEnum {
                name: "PartitionDateSource".to_string(),
                values: vec!["EventTime".to_string(), "DeliveryTime".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Specifies the partition date source for the partitioned prefix. ``PartitionDateSource`` can be ``EventTime`` or ``DeliveryTime``. For ``DeliveryTime``, the time in the log file names corresponds to the delivery time for the log files. For ``EventTime``, The logs delivered are for a specific day only. The year, month, and day correspond to the day on which the event occurred, and the hour, minutes and seconds are set to 00 in the key.").with_provider_name("PartitionDateSource")
                    ],
                }).with_provider_name("PartitionedPrefix"),
                    StructField::new("simple_prefix", AttributeType::Struct {
                    name: "SimplePrefix".to_string(),
                    fields: vec![],
                }).with_description("This format defaults the prefix to the given log file prefix for delivering server access log file.").with_provider_name("SimplePrefix")
                    ],
                }).with_description("Amazon S3 key format for log objects. Only one format, either PartitionedPrefix or SimplePrefix, is allowed.").with_provider_name("TargetObjectKeyFormat")
                    ],
                })
                .with_description("Settings that define where logs are stored.")
                .with_provider_name("LoggingConfiguration"),
        )
        .attribute(
            AttributeSchema::new("metadata_configuration", AttributeType::Struct {
                    name: "MetadataConfiguration".to_string(),
                    fields: vec![
                    StructField::new("destination", AttributeType::Struct {
                    name: "MetadataDestination".to_string(),
                    fields: vec![
                    StructField::new("table_bucket_arn", super::arn()).with_description("The Amazon Resource Name (ARN) of the table bucket where the metadata configuration is stored.").with_provider_name("TableBucketArn"),
                    StructField::new("table_bucket_type", AttributeType::StringEnum {
                name: "TableBucketType".to_string(),
                values: vec!["aws".to_string(), "customer".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The type of the table bucket where the metadata configuration is stored. The ``aws`` value indicates an AWS managed table bucket, and the ``customer`` value indicates a customer-managed table bucket. V2 metadata configurations are stored in AWS managed table buckets, and V1 metadata configurations are stored in customer-managed table buckets.").with_provider_name("TableBucketType"),
                    StructField::new("table_namespace", AttributeType::String).with_description("The namespace in the table bucket where the metadata tables for a metadata configuration are stored.").with_provider_name("TableNamespace")
                    ],
                }).with_description("The destination information for the S3 Metadata configuration.").with_provider_name("Destination"),
                    StructField::new("inventory_table_configuration", AttributeType::Struct {
                    name: "InventoryTableConfiguration".to_string(),
                    fields: vec![
                    StructField::new("configuration_state", AttributeType::StringEnum {
                name: "ConfigurationState".to_string(),
                values: vec!["ENABLED".to_string(), "DISABLED".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The configuration state of the inventory table, indicating whether the inventory table is enabled or disabled.").with_provider_name("ConfigurationState"),
                    StructField::new("encryption_configuration", AttributeType::Struct {
                    name: "MetadataTableEncryptionConfiguration".to_string(),
                    fields: vec![
                    StructField::new("kms_key_arn", super::kms_key_arn()).with_description("If server-side encryption with KMSlong (KMS) keys (SSE-KMS) is specified, you must also specify the KMS key Amazon Resource Name (ARN). You must specify a customer-managed KMS key that's located in the same Region as the general purpose bucket that corresponds to the metadata table configuration.").with_provider_name("KmsKeyArn"),
                    StructField::new("sse_algorithm", AttributeType::StringEnum {
                name: "MetadataTableEncryptionConfigurationSseAlgorithm".to_string(),
                values: vec!["aws:kms".to_string(), "AES256".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The encryption type specified for a metadata table. To specify server-side encryption with KMSlong (KMS) keys (SSE-KMS), use the ``aws:kms`` value. To specify server-side encryption with Amazon S3 managed keys (SSE-S3), use the ``AES256`` value.").with_provider_name("SseAlgorithm")
                    ],
                }).with_description("The encryption configuration for the inventory table.").with_provider_name("EncryptionConfiguration"),
                    StructField::new("table_arn", super::arn()).with_description("The Amazon Resource Name (ARN) for the inventory table.").with_provider_name("TableArn"),
                    StructField::new("table_name", AttributeType::String).with_description("The name of the inventory table.").with_provider_name("TableName")
                    ],
                }).with_description("The inventory table configuration for a metadata configuration.").with_provider_name("InventoryTableConfiguration"),
                    StructField::new("journal_table_configuration", AttributeType::Struct {
                    name: "JournalTableConfiguration".to_string(),
                    fields: vec![
                    StructField::new("encryption_configuration", AttributeType::Struct {
                    name: "MetadataTableEncryptionConfiguration".to_string(),
                    fields: vec![
                    StructField::new("kms_key_arn", super::kms_key_arn()).with_description("If server-side encryption with KMSlong (KMS) keys (SSE-KMS) is specified, you must also specify the KMS key Amazon Resource Name (ARN). You must specify a customer-managed KMS key that's located in the same Region as the general purpose bucket that corresponds to the metadata table configuration.").with_provider_name("KmsKeyArn"),
                    StructField::new("sse_algorithm", AttributeType::StringEnum {
                name: "MetadataTableEncryptionConfigurationSseAlgorithm".to_string(),
                values: vec!["aws:kms".to_string(), "AES256".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The encryption type specified for a metadata table. To specify server-side encryption with KMSlong (KMS) keys (SSE-KMS), use the ``aws:kms`` value. To specify server-side encryption with Amazon S3 managed keys (SSE-S3), use the ``AES256`` value.").with_provider_name("SseAlgorithm")
                    ],
                }).with_description("The encryption configuration for the journal table.").with_provider_name("EncryptionConfiguration"),
                    StructField::new("record_expiration", AttributeType::Struct {
                    name: "RecordExpiration".to_string(),
                    fields: vec![
                    StructField::new("days", AttributeType::Int).with_description("If you enable journal table record expiration, you can set the number of days to retain your journal table records. Journal table records must be retained for a minimum of 7 days. To set this value, specify any whole number from ``7`` to ``2147483647``. For example, to retain your journal table records for one year, set this value to ``365``.").with_provider_name("Days"),
                    StructField::new("expiration", AttributeType::StringEnum {
                name: "Expiration".to_string(),
                values: vec!["ENABLED".to_string(), "DISABLED".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies whether journal table record expiration is enabled or disabled.").with_provider_name("Expiration")
                    ],
                }).required().with_description("The journal table record expiration settings for the journal table.").with_provider_name("RecordExpiration"),
                    StructField::new("table_arn", super::arn()).with_description("The Amazon Resource Name (ARN) for the journal table.").with_provider_name("TableArn"),
                    StructField::new("table_name", AttributeType::String).with_description("The name of the journal table.").with_provider_name("TableName")
                    ],
                }).required().with_description("The journal table configuration for a metadata configuration.").with_provider_name("JournalTableConfiguration")
                    ],
                })
                .with_description("The S3 Metadata configuration for a general purpose bucket.")
                .with_provider_name("MetadataConfiguration"),
        )
        .attribute(
            AttributeSchema::new("metadata_table_configuration", AttributeType::Struct {
                    name: "MetadataTableConfiguration".to_string(),
                    fields: vec![
                    StructField::new("s3_tables_destination", AttributeType::Struct {
                    name: "S3TablesDestination".to_string(),
                    fields: vec![
                    StructField::new("table_arn", super::arn()).with_description("The Amazon Resource Name (ARN) for the metadata table in the metadata table configuration. The specified metadata table name must be unique within the ``aws_s3_metadata`` namespace in the destination table bucket.").with_provider_name("TableArn"),
                    StructField::new("table_bucket_arn", super::arn()).required().with_description("The Amazon Resource Name (ARN) for the table bucket that's specified as the destination in the metadata table configuration. The destination table bucket must be in the same Region and AWS-account as the general purpose bucket.").with_provider_name("TableBucketArn"),
                    StructField::new("table_name", AttributeType::String).required().with_description("The name for the metadata table in your metadata table configuration. The specified metadata table name must be unique within the ``aws_s3_metadata`` namespace in the destination table bucket.").with_provider_name("TableName"),
                    StructField::new("table_namespace", AttributeType::String).with_description("The table bucket namespace for the metadata table in your metadata table configuration. This value is always ``aws_s3_metadata``.").with_provider_name("TableNamespace")
                    ],
                }).required().with_description("The destination information for the metadata table configuration. The destination table bucket must be in the same Region and AWS-account as the general purpose bucket. The specified metadata table name must be unique within the ``aws_s3_metadata`` namespace in the destination table bucket.").with_provider_name("S3TablesDestination")
                    ],
                })
                .with_description("The metadata table configuration of an S3 general purpose bucket.")
                .with_provider_name("MetadataTableConfiguration"),
        )
        .attribute(
            AttributeSchema::new("metrics_configurations", AttributeType::list(AttributeType::Struct {
                    name: "MetricsConfiguration".to_string(),
                    fields: vec![
                    StructField::new("access_point_arn", super::arn()).with_description("The access point that was used while performing operations on the object. The metrics configuration only includes objects that meet the filter's criteria.").with_provider_name("AccessPointArn"),
                    StructField::new("id", AttributeType::String).required().with_description("The ID used to identify the metrics configuration. This can be any value you choose that helps you identify your metrics configuration.").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix that an object must have to be included in the metrics results.").with_provider_name("Prefix"),
                    StructField::new("tag_filters", AttributeType::list(tags_type())).with_description("Specifies a list of tag filters to use as a metrics configuration filter. The metrics configuration includes only objects that meet the filter's criteria.").with_provider_name("TagFilters")
                    ],
                }))
                .with_description("Specifies a metrics configuration for the CloudWatch request metrics (specified by the metrics configuration ID) from an Amazon S3 bucket. If you're updating an existing metrics configuration, note that this is a full replacement of the existing metrics configuration. If you don't include the elements you want to keep, they are erased. For more information, see [PutBucketMetricsConfiguration](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucketPUTMetricConfiguration.html).")
                .with_provider_name("MetricsConfigurations")
                .with_block_name("metrics_configuration"),
        )
        .attribute(
            AttributeSchema::new("notification_configuration", AttributeType::Struct {
                    name: "NotificationConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event_bridge_configuration", AttributeType::Struct {
                    name: "EventBridgeConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event_bridge_enabled", AttributeType::Bool).required().with_description("Enables delivery of events to Amazon EventBridge.").with_provider_name("EventBridgeEnabled")
                    ],
                }).with_description("Enables delivery of events to Amazon EventBridge.").with_provider_name("EventBridgeConfiguration"),
                    StructField::new("lambda_configurations", AttributeType::list(AttributeType::Struct {
                    name: "LambdaConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event", AttributeType::String).required().with_description("The Amazon S3 bucket event for which to invoke the LAMlong function. For more information, see [Supported Event Types](https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html) in the *Amazon S3 User Guide*.").with_provider_name("Event"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "NotificationFilter".to_string(),
                    fields: vec![
                    StructField::new("s3_key", AttributeType::Struct {
                    name: "S3KeyFilter".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::unordered_list(AttributeType::Struct {
                    name: "FilterRule".to_string(),
                    fields: vec![
                    StructField::new("name", AttributeType::Custom {
                name: "String(len: ..=1024)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_1024,
                namespace: None,
                to_dsl: None,
            }).required().with_description("The object key name prefix or suffix identifying one or more objects to which the filtering rule applies. The maximum length is 1,024 characters. Overlapping prefixes and suffixes are not supported. For more information, see [Configuring Event Notifications](https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html) in the *Amazon S3 User Guide*.").with_provider_name("Name"),
                    StructField::new("value", AttributeType::String).required().with_description("The value that the filter searches for in object key names.").with_provider_name("Value")
                    ],
                })).required().with_description("A list of containers for the key-value pair that defines the criteria for the filter rule.").with_provider_name("Rules").with_block_name("rule")
                    ],
                }).required().with_description("A container for object key name prefix and suffix filtering rules.").with_provider_name("S3Key").with_block_name("s3_key")
                    ],
                }).with_description("The filtering rules that determine which objects invoke the AWS Lambda function. For example, you can create a filter so that only image files with a ``.jpg`` extension invoke the function when they are added to the Amazon S3 bucket.").with_provider_name("Filter").with_block_name("filter"),
                    StructField::new("function", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the LAMlong function that Amazon S3 invokes when the specified event type occurs.").with_provider_name("Function")
                    ],
                })).with_description("Describes the LAMlong functions to invoke and the events for which to invoke them.").with_provider_name("LambdaConfigurations").with_block_name("lambda_configuration"),
                    StructField::new("queue_configurations", AttributeType::list(AttributeType::Struct {
                    name: "QueueConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event", AttributeType::String).required().with_description("The Amazon S3 bucket event about which you want to publish messages to Amazon SQS. For more information, see [Supported Event Types](https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html) in the *Amazon S3 User Guide*.").with_provider_name("Event"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "NotificationFilter".to_string(),
                    fields: vec![
                    StructField::new("s3_key", AttributeType::Struct {
                    name: "S3KeyFilter".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::unordered_list(AttributeType::Struct {
                    name: "FilterRule".to_string(),
                    fields: vec![
                    StructField::new("name", AttributeType::Custom {
                name: "String(len: ..=1024)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_1024,
                namespace: None,
                to_dsl: None,
            }).required().with_description("The object key name prefix or suffix identifying one or more objects to which the filtering rule applies. The maximum length is 1,024 characters. Overlapping prefixes and suffixes are not supported. For more information, see [Configuring Event Notifications](https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html) in the *Amazon S3 User Guide*.").with_provider_name("Name"),
                    StructField::new("value", AttributeType::String).required().with_description("The value that the filter searches for in object key names.").with_provider_name("Value")
                    ],
                })).required().with_description("A list of containers for the key-value pair that defines the criteria for the filter rule.").with_provider_name("Rules").with_block_name("rule")
                    ],
                }).required().with_description("A container for object key name prefix and suffix filtering rules.").with_provider_name("S3Key").with_block_name("s3_key")
                    ],
                }).with_description("The filtering rules that determine which objects trigger notifications. For example, you can create a filter so that Amazon S3 sends notifications only when image files with a ``.jpg`` extension are added to the bucket. For more information, see [Configuring event notifications using object key name filtering](https://docs.aws.amazon.com/AmazonS3/latest/user-guide/notification-how-to-filtering.html) in the *Amazon S3 User Guide*.").with_provider_name("Filter").with_block_name("filter"),
                    StructField::new("queue", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the Amazon SQS queue to which Amazon S3 publishes a message when it detects events of the specified type. FIFO queues are not allowed when enabling an SQS queue as the event notification destination.").with_provider_name("Queue")
                    ],
                })).with_description("The Amazon Simple Queue Service queues to publish messages to and the events for which to publish messages.").with_provider_name("QueueConfigurations").with_block_name("queue_configuration"),
                    StructField::new("topic_configurations", AttributeType::list(AttributeType::Struct {
                    name: "TopicConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event", AttributeType::String).required().with_description("The Amazon S3 bucket event about which to send notifications. For more information, see [Supported Event Types](https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html) in the *Amazon S3 User Guide*.").with_provider_name("Event"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "NotificationFilter".to_string(),
                    fields: vec![
                    StructField::new("s3_key", AttributeType::Struct {
                    name: "S3KeyFilter".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::unordered_list(AttributeType::Struct {
                    name: "FilterRule".to_string(),
                    fields: vec![
                    StructField::new("name", AttributeType::Custom {
                name: "String(len: ..=1024)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_1024,
                namespace: None,
                to_dsl: None,
            }).required().with_description("The object key name prefix or suffix identifying one or more objects to which the filtering rule applies. The maximum length is 1,024 characters. Overlapping prefixes and suffixes are not supported. For more information, see [Configuring Event Notifications](https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html) in the *Amazon S3 User Guide*.").with_provider_name("Name"),
                    StructField::new("value", AttributeType::String).required().with_description("The value that the filter searches for in object key names.").with_provider_name("Value")
                    ],
                })).required().with_description("A list of containers for the key-value pair that defines the criteria for the filter rule.").with_provider_name("Rules").with_block_name("rule")
                    ],
                }).required().with_description("A container for object key name prefix and suffix filtering rules.").with_provider_name("S3Key").with_block_name("s3_key")
                    ],
                }).with_description("The filtering rules that determine for which objects to send notifications. For example, you can create a filter so that Amazon S3 sends notifications only when image files with a ``.jpg`` extension are added to the bucket.").with_provider_name("Filter").with_block_name("filter"),
                    StructField::new("topic", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the Amazon SNS topic to which Amazon S3 publishes a message when it detects events of the specified type.").with_provider_name("Topic")
                    ],
                })).with_description("The topic to which notifications are sent and the events for which notifications are generated.").with_provider_name("TopicConfigurations").with_block_name("topic_configuration")
                    ],
                })
                .with_description("Configuration that defines how Amazon S3 handles bucket notifications.")
                .with_provider_name("NotificationConfiguration")
                .with_block_name("notification_configuration"),
        )
        .attribute(
            AttributeSchema::new("object_lock_configuration", AttributeType::Struct {
                    name: "ObjectLockConfiguration".to_string(),
                    fields: vec![
                    StructField::new("object_lock_enabled", AttributeType::StringEnum {
                name: "ObjectLockEnabled".to_string(),
                values: vec!["Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled`` when you apply ``ObjectLockConfiguration`` to a bucket.").with_provider_name("ObjectLockEnabled"),
                    StructField::new("rule", AttributeType::Struct {
                    name: "ObjectLockRule".to_string(),
                    fields: vec![
                    StructField::new("default_retention", AttributeType::Struct {
                    name: "DefaultRetention".to_string(),
                    fields: vec![
                    StructField::new("days", AttributeType::Int).with_description("The number of days that you want to specify for the default retention period. If Object Lock is turned on, you must specify ``Mode`` and specify either ``Days`` or ``Years``.").with_provider_name("Days"),
                    StructField::new("mode", AttributeType::StringEnum {
                name: "Mode".to_string(),
                values: vec!["COMPLIANCE".to_string(), "GOVERNANCE".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("The default Object Lock retention mode you want to apply to new objects placed in the specified bucket. If Object Lock is turned on, you must specify ``Mode`` and specify either ``Days`` or ``Years``.").with_provider_name("Mode"),
                    StructField::new("years", AttributeType::Int).with_description("The number of years that you want to specify for the default retention period. If Object Lock is turned on, you must specify ``Mode`` and specify either ``Days`` or ``Years``.").with_provider_name("Years")
                    ],
                }).with_description("The default Object Lock retention mode and period that you want to apply to new objects placed in the specified bucket. If Object Lock is turned on, bucket settings require both ``Mode`` and a period of either ``Days`` or ``Years``. You cannot specify ``Days`` and ``Years`` at the same time. For more information about allowable values for mode and period, see [DefaultRetention](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-s3-bucket-defaultretention.html).").with_provider_name("DefaultRetention")
                    ],
                }).with_description("Specifies the Object Lock rule for the specified object. Enable this rule when you apply ``ObjectLockConfiguration`` to a bucket. If Object Lock is turned on, bucket settings require both ``Mode`` and a period of either ``Days`` or ``Years``. You cannot specify ``Days`` and ``Years`` at the same time. For more information, see [ObjectLockRule](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-s3-bucket-objectlockrule.html) and [DefaultRetention](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-s3-bucket-defaultretention.html).").with_provider_name("Rule")
                    ],
                })
                .with_description("This operation is not supported for directory buckets. Places an Object Lock configuration on the specified bucket. The rule specified in the Object Lock configuration will be applied by default to every new object placed in the specified bucket. For more information, see [Locking Objects](https://docs.aws.amazon.com/AmazonS3/latest/dev/object-lock.html). + The ``DefaultRetention`` settings require both a mode and a period. + The ``DefaultRetention`` period can be either ``Days`` or ``Years`` but you must select one. You cannot specify ``Days`` and ``Years`` at the same time. + You can enable Object Lock for new or existing buckets. For more information, see [Configuring Object Lock](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock-configure.html). You must URL encode any signed header values that contain spaces. For example, if your header value is ``my file.txt``, containing two spaces after ``my``, you must URL encode this value to ``my%20%20file.txt``.")
                .with_provider_name("ObjectLockConfiguration"),
        )
        .attribute(
            AttributeSchema::new("object_lock_enabled", AttributeType::Bool)
                .with_description("Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled`` when you apply ``ObjectLockConfiguration`` to a bucket.")
                .with_provider_name("ObjectLockEnabled"),
        )
        .attribute(
            AttributeSchema::new("ownership_controls", AttributeType::Struct {
                    name: "OwnershipControls".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::list(AttributeType::Struct {
                    name: "OwnershipControlsRule".to_string(),
                    fields: vec![
                    StructField::new("object_ownership", AttributeType::StringEnum {
                name: "ObjectOwnership".to_string(),
                values: vec!["ObjectWriter".to_string(), "BucketOwnerPreferred".to_string(), "BucketOwnerEnforced".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Specifies an object ownership rule.").with_provider_name("ObjectOwnership")
                    ],
                })).required().with_description("Specifies the container element for Object Ownership rules.").with_provider_name("Rules").with_block_name("rule")
                    ],
                })
                .with_description("Configuration that defines how Amazon S3 handles Object Ownership rules.")
                .with_provider_name("OwnershipControls")
                .with_block_name("ownership_control"),
        )
        .attribute(
            AttributeSchema::new("public_access_block_configuration", AttributeType::Struct {
                    name: "PublicAccessBlockConfiguration".to_string(),
                    fields: vec![
                    StructField::new("block_public_acls", AttributeType::Bool).with_description("Specifies whether Amazon S3 should block public access control lists (ACLs) for this bucket and objects in this bucket. Setting this element to ``TRUE`` causes the following behavior: + PUT Bucket ACL and PUT Object ACL calls fail if the specified ACL is public. + PUT Object calls fail if the request includes a public ACL. + PUT Bucket calls fail if the request includes a public ACL. Enabling this setting doesn't affect existing policies or ACLs.").with_provider_name("BlockPublicAcls"),
                    StructField::new("block_public_policy", AttributeType::Bool).with_description("Specifies whether Amazon S3 should block public bucket policies for this bucket. Setting this element to ``TRUE`` causes Amazon S3 to reject calls to PUT Bucket policy if the specified bucket policy allows public access. Enabling this setting doesn't affect existing bucket policies.").with_provider_name("BlockPublicPolicy"),
                    StructField::new("ignore_public_acls", AttributeType::Bool).with_description("Specifies whether Amazon S3 should ignore public ACLs for this bucket and objects in this bucket. Setting this element to ``TRUE`` causes Amazon S3 to ignore all public ACLs on this bucket and objects in this bucket. Enabling this setting doesn't affect the persistence of any existing ACLs and doesn't prevent new public ACLs from being set.").with_provider_name("IgnorePublicAcls"),
                    StructField::new("restrict_public_buckets", AttributeType::Bool).with_description("Specifies whether Amazon S3 should restrict public bucket policies for this bucket. Setting this element to ``TRUE`` restricts access to this bucket to only AWS-service principals and authorized users within this account if the bucket has a public policy. Enabling this setting doesn't affect previously stored bucket policies, except that public and cross-account access within any public bucket policy, including non-public delegation to specific accounts, is blocked.").with_provider_name("RestrictPublicBuckets")
                    ],
                })
                .with_description("Configuration that defines how Amazon S3 handles public access.")
                .with_provider_name("PublicAccessBlockConfiguration"),
        )
        .attribute(
            AttributeSchema::new("regional_domain_name", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("RegionalDomainName"),
        )
        .attribute(
            AttributeSchema::new("replication_configuration", AttributeType::Struct {
                    name: "ReplicationConfiguration".to_string(),
                    fields: vec![
                    StructField::new("role", super::iam_role_arn()).required().with_description("The Amazon Resource Name (ARN) of the IAMlong (IAM) role that Amazon S3 assumes when replicating objects. For more information, see [How to Set Up Replication](https://docs.aws.amazon.com/AmazonS3/latest/dev/replication-how-setup.html) in the *Amazon S3 User Guide*.").with_provider_name("Role"),
                    StructField::new("rules", AttributeType::list(AttributeType::Struct {
                    name: "ReplicationRule".to_string(),
                    fields: vec![
                    StructField::new("delete_marker_replication", AttributeType::Struct {
                    name: "DeleteMarkerReplication".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::StringEnum {
                name: "DeleteMarkerReplicationStatus".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Indicates whether to replicate delete markers.").with_provider_name("Status")
                    ],
                }).with_description("Specifies whether Amazon S3 replicates delete markers. If you specify a ``Filter`` in your replication configuration, you must also include a ``DeleteMarkerReplication`` element. If your ``Filter`` includes a ``Tag`` element, the ``DeleteMarkerReplication````Status`` must be set to Disabled, because Amazon S3 does not support replicating delete markers for tag-based rules. For an example configuration, see [Basic Rule Configuration](https://docs.aws.amazon.com/AmazonS3/latest/dev/replication-add-config.html#replication-config-min-rule-config). For more information about delete marker replication, see [Basic Rule Configuration](https://docs.aws.amazon.com/AmazonS3/latest/dev/delete-marker-replication.html). If you are using an earlier version of the replication configuration, Amazon S3 handles replication of delete markers differently. For more information, see [Backward Compatibility](https://docs.aws.amazon.com/AmazonS3/latest/dev/replication-add-config.html#replication-backward-compat-considerations).").with_provider_name("DeleteMarkerReplication"),
                    StructField::new("destination", AttributeType::Struct {
                    name: "ReplicationDestination".to_string(),
                    fields: vec![
                    StructField::new("access_control_translation", AttributeType::Struct {
                    name: "AccessControlTranslation".to_string(),
                    fields: vec![
                    StructField::new("owner", AttributeType::StringEnum {
                name: "Owner".to_string(),
                values: vec!["Destination".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies the replica ownership. For default and valid values, see [PUT bucket replication](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucketPUTreplication.html) in the *Amazon S3 API Reference*.").with_provider_name("Owner")
                    ],
                }).with_description("Specify this only in a cross-account scenario (where source and destination bucket owners are not the same), and you want to change replica ownership to the AWS-account that owns the destination bucket. If this is not specified in the replication configuration, the replicas are owned by same AWS-account that owns the source object.").with_provider_name("AccessControlTranslation"),
                    StructField::new("account", super::aws_account_id()).with_description("Destination bucket owner account ID. In a cross-account scenario, if you direct Amazon S3 to change replica ownership to the AWS-account that owns the destination bucket by specifying the ``AccessControlTranslation`` property, this is the account ID of the destination bucket owner. For more information, see [Cross-Region Replication Additional Configuration: Change Replica Owner](https://docs.aws.amazon.com/AmazonS3/latest/dev/crr-change-owner.html) in the *Amazon S3 User Guide*. If you specify the ``AccessControlTranslation`` property, the ``Account`` property is required.").with_provider_name("Account"),
                    StructField::new("bucket", AttributeType::String).required().with_description("The Amazon Resource Name (ARN) of the bucket where you want Amazon S3 to store the results.").with_provider_name("Bucket"),
                    StructField::new("encryption_configuration", AttributeType::Struct {
                    name: "EncryptionConfiguration".to_string(),
                    fields: vec![
                    StructField::new("replica_kms_key_id", super::kms_key_id()).required().with_description("Specifies the ID (Key ARN or Alias ARN) of the customer managed AWS KMS key stored in AWS Key Management Service (KMS) for the destination bucket. Amazon S3 uses this key to encrypt replica objects. Amazon S3 only supports symmetric encryption KMS keys. For more information, see [Asymmetric keys in KMS](https://docs.aws.amazon.com//kms/latest/developerguide/symmetric-asymmetric.html) in the *Key Management Service Developer Guide*.").with_provider_name("ReplicaKmsKeyID")
                    ],
                }).with_description("Specifies encryption-related information.").with_provider_name("EncryptionConfiguration"),
                    StructField::new("metrics", AttributeType::Struct {
                    name: "Metrics".to_string(),
                    fields: vec![
                    StructField::new("event_threshold", AttributeType::Struct {
                    name: "ReplicationTimeValue".to_string(),
                    fields: vec![
                    StructField::new("minutes", AttributeType::Int).required().with_description("Contains an integer specifying time in minutes. Valid value: 15").with_provider_name("Minutes")
                    ],
                }).with_description("A container specifying the time threshold for emitting the ``s3:Replication:OperationMissedThreshold`` event.").with_provider_name("EventThreshold"),
                    StructField::new("status", AttributeType::StringEnum {
                name: "MetricsStatus".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies whether the replication metrics are enabled.").with_provider_name("Status")
                    ],
                }).with_description("A container specifying replication metrics-related settings enabling replication metrics and events.").with_provider_name("Metrics"),
                    StructField::new("replication_time", AttributeType::Struct {
                    name: "ReplicationTime".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::StringEnum {
                name: "ReplicationTimeStatus".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies whether the replication time is enabled.").with_provider_name("Status"),
                    StructField::new("time", AttributeType::Struct {
                    name: "ReplicationTimeValue".to_string(),
                    fields: vec![
                    StructField::new("minutes", AttributeType::Int).required().with_description("Contains an integer specifying time in minutes. Valid value: 15").with_provider_name("Minutes")
                    ],
                }).required().with_description("A container specifying the time by which replication should be complete for all objects and operations on objects.").with_provider_name("Time")
                    ],
                }).with_description("A container specifying S3 Replication Time Control (S3 RTC), including whether S3 RTC is enabled and the time when all objects and operations on objects must be replicated. Must be specified together with a ``Metrics`` block.").with_provider_name("ReplicationTime"),
                    StructField::new("storage_class", AttributeType::StringEnum {
                name: "ReplicationDestinationStorageClass".to_string(),
                values: vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "REDUCED_REDUNDANCY".to_string(), "STANDARD".to_string(), "STANDARD_IA".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("The storage class to use when replicating objects, such as S3 Standard or reduced redundancy. By default, Amazon S3 uses the storage class of the source object to create the object replica. For valid values, see the ``StorageClass`` element of the [PUT Bucket replication](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucketPUTreplication.html) action in the *Amazon S3 API Reference*. ``FSX_OPENZFS`` is not an accepted value when replicating objects.").with_provider_name("StorageClass")
                    ],
                }).required().with_description("A container for information about the replication destination and its configurations including enabling the S3 Replication Time Control (S3 RTC).").with_provider_name("Destination"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "ReplicationRuleFilter".to_string(),
                    fields: vec![
                    StructField::new("and", AttributeType::Struct {
                    name: "ReplicationRuleAndOperator".to_string(),
                    fields: vec![
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the subset of objects to which the rule applies.").with_provider_name("Prefix"),
                    StructField::new("tag_filters", AttributeType::list(tags_type())).with_description("An array of tags containing key and value pairs.").with_provider_name("TagFilters")
                    ],
                }).with_description("A container for specifying rule filters. The filters determine the subset of objects to which the rule applies. This element is required only if you specify more than one filter. For example: + If you specify both a ``Prefix`` and a ``TagFilter``, wrap these filters in an ``And`` tag. + If you specify a filter based on multiple tags, wrap the ``TagFilter`` elements in an ``And`` tag.").with_provider_name("And"),
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the subset of objects to which the rule applies. Replacement must be made for object keys containing special characters (such as carriage returns) when using XML requests. For more information, see [XML related object key constraints](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html#object-key-xml-related-constraints).").with_provider_name("Prefix"),
                    StructField::new("tag_filter", tags_type()).with_description("A container for specifying a tag key and value. The rule applies only to objects that have the tag in their tag set.").with_provider_name("TagFilter")
                    ],
                }).with_description("A filter that identifies the subset of objects to which the replication rule applies. A ``Filter`` must specify exactly one ``Prefix``, ``TagFilter``, or an ``And`` child element. The use of the filter field indicates that this is a V2 replication configuration. This field isn't supported in a V1 replication configuration. V1 replication configuration only supports filtering by key prefix. To filter using a V1 replication configuration, add the ``Prefix`` directly as a child element of the ``Rule`` element.").with_provider_name("Filter"),
                    StructField::new("id", AttributeType::Custom {
                name: "String(len: ..=255)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_255,
                namespace: None,
                to_dsl: None,
            }).with_description("A unique identifier for the rule. The maximum value is 255 characters. If you don't specify a value, AWS CloudFormation generates a random ID. When using a V2 replication configuration this property is capitalized as \"ID\".").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::Custom {
                name: "String(len: ..=1024)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_max_1024,
                namespace: None,
                to_dsl: None,
            }).with_description("An object key name prefix that identifies the object or objects to which the rule applies. The maximum prefix length is 1,024 characters. To include all objects in a bucket, specify an empty string. To filter using a V1 replication configuration, add the ``Prefix`` directly as a child element of the ``Rule`` element. Replacement must be made for object keys containing special characters (such as carriage returns) when using XML requests. For more information, see [XML related object key constraints](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html#object-key-xml-related-constraints).").with_provider_name("Prefix"),
                    StructField::new("priority", AttributeType::Int).with_description("The priority indicates which rule has precedence whenever two or more replication rules conflict. Amazon S3 will attempt to replicate objects according to all replication rules. However, if there are two or more rules with the same destination bucket, then objects will be replicated according to the rule with the highest priority. The higher the number, the higher the priority. For more information, see [Replication](https://docs.aws.amazon.com/AmazonS3/latest/dev/replication.html) in the *Amazon S3 User Guide*.").with_provider_name("Priority"),
                    StructField::new("source_selection_criteria", AttributeType::Struct {
                    name: "SourceSelectionCriteria".to_string(),
                    fields: vec![
                    StructField::new("replica_modifications", AttributeType::Struct {
                    name: "ReplicaModifications".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::StringEnum {
                name: "ReplicaModificationsStatus".to_string(),
                values: vec!["Enabled".to_string(), "Disabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies whether Amazon S3 replicates modifications on replicas. *Allowed values*: ``Enabled`` | ``Disabled``").with_provider_name("Status")
                    ],
                }).with_description("A filter that you can specify for selection for modifications on replicas.").with_provider_name("ReplicaModifications"),
                    StructField::new("sse_kms_encrypted_objects", AttributeType::Struct {
                    name: "SseKmsEncryptedObjects".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::StringEnum {
                name: "SseKmsEncryptedObjectsStatus".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies whether Amazon S3 replicates objects created with server-side encryption using an AWS KMS key stored in AWS Key Management Service.").with_provider_name("Status")
                    ],
                }).with_description("A container for filter information for the selection of Amazon S3 objects encrypted with AWS KMS.").with_provider_name("SseKmsEncryptedObjects")
                    ],
                }).with_description("A container that describes additional filters for identifying the source objects that you want to replicate. You can choose to enable or disable the replication of these objects.").with_provider_name("SourceSelectionCriteria"),
                    StructField::new("status", AttributeType::StringEnum {
                name: "ReplicationRuleStatus".to_string(),
                values: vec!["Disabled".to_string(), "Enabled".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("Specifies whether the rule is enabled.").with_provider_name("Status")
                    ],
                })).required().with_description("A container for one or more replication rules. A replication configuration must have at least one rule and can contain a maximum of 1,000 rules.").with_provider_name("Rules").with_block_name("rule")
                    ],
                })
                .with_description("Configuration for replicating objects in an S3 bucket. To enable replication, you must also enable versioning by using the ``VersioningConfiguration`` property. Amazon S3 can store replicated objects in a single destination bucket or multiple destination buckets. The destination bucket or buckets must already exist.")
                .with_provider_name("ReplicationConfiguration")
                .with_block_name("replication_configuration"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("An arbitrary set of tags (key-value pairs) for this S3 bucket.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("versioning_configuration", AttributeType::Struct {
                    name: "VersioningConfiguration".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::StringEnum {
                name: "VersioningConfigurationStatus".to_string(),
                values: vec!["Enabled".to_string(), "Suspended".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).required().with_description("The versioning state of the bucket.").with_provider_name("Status")
                    ],
                })
                .with_description("Enables multiple versions of all objects in this bucket. You might enable versioning to prevent objects from being deleted or overwritten by mistake or to archive objects so that you can retrieve previous versions of them. When you enable versioning on a bucket for the first time, it might take a short amount of time for the change to be fully propagated. We recommend that you wait for 15 minutes after enabling versioning before issuing write operations (``PUT`` or ``DELETE``) on objects in the bucket.")
                .with_provider_name("VersioningConfiguration"),
        )
        .attribute(
            AttributeSchema::new("website_configuration", AttributeType::Struct {
                    name: "WebsiteConfiguration".to_string(),
                    fields: vec![
                    StructField::new("error_document", AttributeType::String).with_description("The name of the error document for the website.").with_provider_name("ErrorDocument"),
                    StructField::new("index_document", AttributeType::String).with_description("The name of the index document for the website.").with_provider_name("IndexDocument"),
                    StructField::new("redirect_all_requests_to", AttributeType::Struct {
                    name: "RedirectAllRequestsTo".to_string(),
                    fields: vec![
                    StructField::new("host_name", AttributeType::String).required().with_description("Name of the host where requests are redirected.").with_provider_name("HostName"),
                    StructField::new("protocol", AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["http".to_string(), "https".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Protocol to use when redirecting requests. The default is the protocol that is used in the original request.").with_provider_name("Protocol")
                    ],
                }).with_description("The redirect behavior for every request to this bucket's website endpoint. If you specify this property, you can't specify any other property.").with_provider_name("RedirectAllRequestsTo"),
                    StructField::new("routing_rules", AttributeType::list(AttributeType::Struct {
                    name: "RoutingRule".to_string(),
                    fields: vec![
                    StructField::new("redirect_rule", AttributeType::Struct {
                    name: "RedirectRule".to_string(),
                    fields: vec![
                    StructField::new("host_name", AttributeType::String).with_description("The host name to use in the redirect request.").with_provider_name("HostName"),
                    StructField::new("http_redirect_code", AttributeType::String).with_description("The HTTP redirect code to use on the response. Not required if one of the siblings is present.").with_provider_name("HttpRedirectCode"),
                    StructField::new("protocol", AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["http".to_string(), "https".to_string()],
                namespace: Some("awscc.s3.bucket".to_string()),
                to_dsl: None,
            }).with_description("Protocol to use when redirecting requests. The default is the protocol that is used in the original request.").with_provider_name("Protocol"),
                    StructField::new("replace_key_prefix_with", AttributeType::String).with_description("The object key prefix to use in the redirect request. For example, to redirect requests for all pages with prefix ``docs/`` (objects in the ``docs/`` folder) to ``documents/``, you can set a condition block with ``KeyPrefixEquals`` set to ``docs/`` and in the Redirect set ``ReplaceKeyPrefixWith`` to ``/documents``. Not required if one of the siblings is present. Can be present only if ``ReplaceKeyWith`` is not provided. Replacement must be made for object keys containing special characters (such as carriage returns) when using XML requests. For more information, see [XML related object key constraints](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html#object-key-xml-related-constraints).").with_provider_name("ReplaceKeyPrefixWith"),
                    StructField::new("replace_key_with", AttributeType::String).with_description("The specific object key to use in the redirect request. For example, redirect request to ``error.html``. Not required if one of the siblings is present. Can be present only if ``ReplaceKeyPrefixWith`` is not provided. Replacement must be made for object keys containing special characters (such as carriage returns) when using XML requests. For more information, see [XML related object key constraints](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html#object-key-xml-related-constraints).").with_provider_name("ReplaceKeyWith")
                    ],
                }).required().with_description("Container for redirect information. You can redirect requests to another host, to another page, or with another protocol. In the event of an error, you can specify a different error code to return.").with_provider_name("RedirectRule"),
                    StructField::new("routing_rule_condition", AttributeType::Struct {
                    name: "RoutingRuleCondition".to_string(),
                    fields: vec![
                    StructField::new("http_error_code_returned_equals", AttributeType::String).with_description("The HTTP error code when the redirect is applied. In the event of an error, if the error code equals this value, then the specified redirect is applied. Required when parent element ``Condition`` is specified and sibling ``KeyPrefixEquals`` is not specified. If both are specified, then both must be true for the redirect to be applied.").with_provider_name("HttpErrorCodeReturnedEquals"),
                    StructField::new("key_prefix_equals", AttributeType::String).with_description("The object key name prefix when the redirect is applied. For example, to redirect requests for ``ExamplePage.html``, the key prefix will be ``ExamplePage.html``. To redirect request for all pages with the prefix ``docs/``, the key prefix will be ``docs/``, which identifies all objects in the docs/ folder. Required when the parent element ``Condition`` is specified and sibling ``HttpErrorCodeReturnedEquals`` is not specified. If both conditions are specified, both must be true for the redirect to be applied.").with_provider_name("KeyPrefixEquals")
                    ],
                }).with_description("A container for describing a condition that must be met for the specified redirect to apply. For example, 1. If request is for pages in the ``/docs`` folder, redirect to the ``/documents`` folder. 2. If request results in HTTP error 4xx, redirect request to another host where you might process the error.").with_provider_name("RoutingRuleCondition")
                    ],
                })).with_description("Rules that define when a redirect is applied and the redirect behavior.").with_provider_name("RoutingRules").with_block_name("routing_rule")
                    ],
                })
                .with_description("Information used to configure the bucket as a static website. For more information, see [Hosting Websites on Amazon S3](https://docs.aws.amazon.com/AmazonS3/latest/dev/WebsiteHosting.html).")
                .with_provider_name("WebsiteConfiguration")
                .with_block_name("website_configuration"),
        )
        .attribute(
            AttributeSchema::new("website_url", AttributeType::Custom {
                name: "String(uri)".to_string(),
                base: Box::new(AttributeType::String),
                validate: |_| Ok(()),
                namespace: None,
                to_dsl: None,
            })
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("WebsiteURL"),
        )
        .with_name_attribute("bucket_name")
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
            ("abac_status", VALID_ABAC_STATUS),
            (
                "acceleration_status",
                VALID_ACCELERATE_CONFIGURATION_ACCELERATION_STATUS,
            ),
            ("access_control", VALID_ACCESS_CONTROL),
            ("owner", VALID_ACCESS_CONTROL_TRANSLATION_OWNER),
            (
                "encryption_type",
                VALID_BLOCKED_ENCRYPTION_TYPES_ENCRYPTION_TYPE,
            ),
            ("allowed_methods", VALID_CORS_RULE_ALLOWED_METHODS),
            (
                "output_schema_version",
                VALID_DATA_EXPORT_OUTPUT_SCHEMA_VERSION,
            ),
            ("mode", VALID_DEFAULT_RETENTION_MODE),
            ("status", VALID_DELETE_MARKER_REPLICATION_STATUS),
            ("format", VALID_DESTINATION_FORMAT),
            ("status", VALID_INTELLIGENT_TIERING_CONFIGURATION_STATUS),
            (
                "included_object_versions",
                VALID_INVENTORY_CONFIGURATION_INCLUDED_OBJECT_VERSIONS,
            ),
            (
                "optional_fields",
                VALID_INVENTORY_CONFIGURATION_OPTIONAL_FIELDS,
            ),
            (
                "schedule_frequency",
                VALID_INVENTORY_CONFIGURATION_SCHEDULE_FREQUENCY,
            ),
            (
                "configuration_state",
                VALID_INVENTORY_TABLE_CONFIGURATION_CONFIGURATION_STATE,
            ),
            (
                "transition_default_minimum_object_size",
                VALID_LIFECYCLE_CONFIGURATION_TRANSITION_DEFAULT_MINIMUM_OBJECT_SIZE,
            ),
            (
                "table_bucket_type",
                VALID_METADATA_DESTINATION_TABLE_BUCKET_TYPE,
            ),
            (
                "sse_algorithm",
                VALID_METADATA_TABLE_ENCRYPTION_CONFIGURATION_SSE_ALGORITHM,
            ),
            ("status", VALID_METRICS_STATUS),
            (
                "storage_class",
                VALID_NONCURRENT_VERSION_TRANSITION_STORAGE_CLASS,
            ),
            (
                "object_lock_enabled",
                VALID_OBJECT_LOCK_CONFIGURATION_OBJECT_LOCK_ENABLED,
            ),
            (
                "object_ownership",
                VALID_OWNERSHIP_CONTROLS_RULE_OBJECT_OWNERSHIP,
            ),
            (
                "partition_date_source",
                VALID_PARTITIONED_PREFIX_PARTITION_DATE_SOURCE,
            ),
            ("expiration", VALID_RECORD_EXPIRATION_EXPIRATION),
            ("protocol", VALID_REDIRECT_ALL_REQUESTS_TO_PROTOCOL),
            ("protocol", VALID_REDIRECT_RULE_PROTOCOL),
            ("status", VALID_REPLICA_MODIFICATIONS_STATUS),
            ("storage_class", VALID_REPLICATION_DESTINATION_STORAGE_CLASS),
            ("status", VALID_REPLICATION_RULE_STATUS),
            ("status", VALID_REPLICATION_TIME_STATUS),
            ("status", VALID_RULE_STATUS),
            (
                "sse_algorithm",
                VALID_SERVER_SIDE_ENCRYPTION_BY_DEFAULT_SSE_ALGORITHM,
            ),
            ("status", VALID_SSE_KMS_ENCRYPTED_OBJECTS_STATUS),
            ("access_tier", VALID_TIERING_ACCESS_TIER),
            ("storage_class", VALID_TRANSITION_STORAGE_CLASS),
            ("status", VALID_VERSIONING_CONFIGURATION_STATUS),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
