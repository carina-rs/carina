//! bucket schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::S3::Bucket
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

const VALID_ABAC_STATUS: &[&str] = &["Enabled", "Disabled"];

fn validate_abac_status(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(value, "AbacStatus", "awscc.s3_bucket", VALID_ABAC_STATUS)
}

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

fn validate_access_control(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "AccessControl",
        "awscc.s3_bucket",
        VALID_ACCESS_CONTROL,
    )
}

/// Returns the schema config for s3_bucket (AWS::S3::Bucket)
pub fn s3_bucket_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::S3::Bucket",
        resource_type_name: "s3_bucket",
        has_tags: true,
        schema: ResourceSchema::new("awscc.s3_bucket")
        .with_description("The ``AWS::S3::Bucket`` resource creates an Amazon S3 bucket in the same AWS Region where you create the AWS CloudFormation stack.  To control how AWS CloudFormation handles the bucket when the stack ...")
        .attribute(
            AttributeSchema::new("abac_status", AttributeType::Custom {
                name: "AbacStatus".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_abac_status,
                namespace: Some("awscc.s3_bucket".to_string()),
            })
                .with_description("The ABAC status of the general purpose bucket. When ABAC is enabled for the general purpose bucket, you can use tags to manage access to the general p...")
                .with_provider_name("AbacStatus"),
        )
        .attribute(
            AttributeSchema::new("accelerate_configuration", AttributeType::Struct {
                    name: "AccelerateConfiguration".to_string(),
                    fields: vec![
                    StructField::new("acceleration_status", AttributeType::Enum(vec!["Enabled".to_string(), "Suspended".to_string()])).required().with_description("Specifies the transfer acceleration status of the bucket.").with_provider_name("AccelerationStatus")
                    ],
                })
                .with_description("Configures the transfer acceleration state for an Amazon S3 bucket. For more information, see [Amazon S3 Transfer Acceleration](https://docs.aws.amazo...")
                .with_provider_name("AccelerateConfiguration"),
        )
        .attribute(
            AttributeSchema::new("access_control", AttributeType::Custom {
                name: "AccessControl".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_access_control,
                namespace: Some("awscc.s3_bucket".to_string()),
            })
                .with_description("This is a legacy property, and it is not recommended for most use cases. A majority of modern use cases in Amazon S3 no longer require the use of ACLs...")
                .with_provider_name("AccessControl"),
        )
        .attribute(
            AttributeSchema::new("analytics_configurations", AttributeType::List(Box::new(AttributeType::Struct {
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
                    StructField::new("bucket_account_id", AttributeType::String).with_description("The account ID that owns the destination S3 bucket. If no account ID is provided, the owner is not validated before exporting data.  Although this val...").with_provider_name("BucketAccountId"),
                    StructField::new("bucket_arn", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the bucket to which data is exported.").with_provider_name("BucketArn"),
                    StructField::new("format", AttributeType::Enum(vec!["CSV".to_string(), "ORC".to_string(), "Parquet".to_string()])).required().with_description("Specifies the file format used when exporting data to Amazon S3. *Allowed values*: ``CSV`` | ``ORC`` | ``Parquet``").with_provider_name("Format"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix to use when exporting data. The prefix is prepended to all results.").with_provider_name("Prefix")
                    ],
                }).required().with_description("The place to store the data for an analysis.").with_provider_name("Destination"),
                    StructField::new("output_schema_version", AttributeType::String).required().with_description("The version of the output schema to use when exporting data. Must be ``V_1``.").with_provider_name("OutputSchemaVersion")
                    ],
                }).with_description("Specifies how data related to the storage class analysis for an Amazon S3 bucket should be exported.").with_provider_name("DataExport")
                    ],
                }).required().with_description("Contains data related to access patterns to be collected and made available to analyze the tradeoffs between different storage classes.").with_provider_name("StorageClassAnalysis"),
                    StructField::new("tag_filters", AttributeType::List(Box::new(tags_type()))).with_description("The tags to use when evaluating an analytics filter. The analytics only includes objects that meet the filter's criteria. If no filter is specified, a...").with_provider_name("TagFilters")
                    ],
                })))
                .with_description("Specifies the configuration and any analyses for the analytics filter of an Amazon S3 bucket.")
                .with_provider_name("AnalyticsConfigurations"),
        )
        .attribute(
            AttributeSchema::new("arn", super::arn())
                .with_description(" (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("bucket_encryption", AttributeType::Struct {
                    name: "BucketEncryption".to_string(),
                    fields: vec![
                    StructField::new("server_side_encryption_configuration", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "ServerSideEncryptionRule".to_string(),
                    fields: vec![
                    StructField::new("blocked_encryption_types", AttributeType::Struct {
                    name: "BlockedEncryptionTypes".to_string(),
                    fields: vec![
                    StructField::new("encryption_type", AttributeType::String).with_description("The object encryption type that you want to block or unblock for an Amazon S3 general purpose bucket.  Currently, this parameter only supports blockin...").with_provider_name("EncryptionType")
                    ],
                }).with_description("A bucket-level setting for Amazon S3 general purpose buckets used to prevent the upload of new objects encrypted with the specified server-side encryp...").with_provider_name("BlockedEncryptionTypes"),
                    StructField::new("bucket_key_enabled", AttributeType::Bool).with_description("Specifies whether Amazon S3 should use an S3 Bucket Key with server-side encryption using KMS (SSE-KMS) for new objects in the bucket. Existing object...").with_provider_name("BucketKeyEnabled"),
                    StructField::new("server_side_encryption_by_default", AttributeType::Struct {
                    name: "ServerSideEncryptionByDefault".to_string(),
                    fields: vec![
                    StructField::new("kms_master_key_id", super::kms_key_arn()).with_description("AWS Key Management Service (KMS) customer managed key ID to use for the default encryption.   + *General purpose buckets* - This parameter is allowed ...").with_provider_name("KMSMasterKeyID"),
                    StructField::new("sse_algorithm", AttributeType::Enum(vec!["aws:kms".to_string(), "AES256".to_string(), "aws:kms:dsse".to_string()])).required().with_description("Server-side encryption algorithm to use for the default encryption.  For directory buckets, there are only two supported values for server-side encryp...").with_provider_name("SSEAlgorithm")
                    ],
                }).with_description("Specifies the default server-side encryption to apply to new objects in the bucket. If a PUT Object request doesn't specify any server-side encryption...").with_provider_name("ServerSideEncryptionByDefault")
                    ],
                }))).required().with_description("Specifies the default server-side-encryption configuration.").with_provider_name("ServerSideEncryptionConfiguration")
                    ],
                })
                .with_description("Specifies default encryption for a bucket using server-side encryption with Amazon S3-managed keys (SSE-S3), AWS KMS-managed keys (SSE-KMS), or dual-l...")
                .with_provider_name("BucketEncryption"),
        )
        .attribute(
            AttributeSchema::new("bucket_name", AttributeType::String)
                .create_only()
                .with_description("A name for the bucket. If you don't specify a name, AWS CloudFormation generates a unique ID and uses that ID for the bucket name. The bucket name mus...")
                .with_provider_name("BucketName"),
        )
        .attribute(
            AttributeSchema::new("cors_configuration", AttributeType::Struct {
                    name: "CorsConfiguration".to_string(),
                    fields: vec![
                    StructField::new("cors_rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "CorsRule".to_string(),
                    fields: vec![
                    StructField::new("allowed_headers", AttributeType::List(Box::new(AttributeType::String))).with_description("Headers that are specified in the ``Access-Control-Request-Headers`` header. These headers are allowed in a preflight OPTIONS request. In response to ...").with_provider_name("AllowedHeaders"),
                    StructField::new("allowed_methods", AttributeType::List(Box::new(AttributeType::String))).required().with_description("An HTTP method that you allow the origin to run. *Allowed values*: ``GET`` | ``PUT`` | ``HEAD`` | ``POST`` | ``DELETE``").with_provider_name("AllowedMethods"),
                    StructField::new("allowed_origins", AttributeType::List(Box::new(AttributeType::String))).required().with_description("One or more origins you want customers to be able to access the bucket from.").with_provider_name("AllowedOrigins"),
                    StructField::new("exposed_headers", AttributeType::List(Box::new(AttributeType::String))).with_description("One or more headers in the response that you want customers to be able to access from their applications (for example, from a JavaScript ``XMLHttpRequ...").with_provider_name("ExposedHeaders"),
                    StructField::new("id", AttributeType::String).with_description("A unique identifier for this rule. The value must be no more than 255 characters.").with_provider_name("Id"),
                    StructField::new("max_age", AttributeType::Int).with_description("The time in seconds that your browser is to cache the preflight response for the specified resource.").with_provider_name("MaxAge")
                    ],
                }))).required().with_description("A set of origins and methods (cross-origin access that you want to allow). You can add up to 100 rules to the configuration.").with_provider_name("CorsRules")
                    ],
                })
                .with_description("Describes the cross-origin access configuration for objects in an Amazon S3 bucket. For more information, see [Enabling Cross-Origin Resource Sharing]...")
                .with_provider_name("CorsConfiguration"),
        )
        .attribute(
            AttributeSchema::new("domain_name", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("DomainName"),
        )
        .attribute(
            AttributeSchema::new("dual_stack_domain_name", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("DualStackDomainName"),
        )
        .attribute(
            AttributeSchema::new("intelligent_tiering_configurations", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "IntelligentTieringConfiguration".to_string(),
                    fields: vec![
                    StructField::new("id", AttributeType::String).required().with_description("The ID used to identify the S3 Intelligent-Tiering configuration.").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the subset of objects to which the rule applies.").with_provider_name("Prefix"),
                    StructField::new("status", AttributeType::Enum(vec!["Disabled".to_string(), "Enabled".to_string()])).required().with_description("Specifies the status of the configuration.").with_provider_name("Status"),
                    StructField::new("tag_filters", AttributeType::List(Box::new(tags_type()))).with_description("A container for a key-value pair.").with_provider_name("TagFilters"),
                    StructField::new("tierings", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Tiering".to_string(),
                    fields: vec![
                    StructField::new("access_tier", AttributeType::Enum(vec!["ARCHIVE_ACCESS".to_string(), "DEEP_ARCHIVE_ACCESS".to_string()])).required().with_description("S3 Intelligent-Tiering access tier. See [Storage class for automatically optimizing frequently and infrequently accessed objects](https://docs.aws.ama...").with_provider_name("AccessTier"),
                    StructField::new("days", AttributeType::Int).required().with_description("The number of consecutive days of no access after which an object will be eligible to be transitioned to the corresponding tier. The minimum number of...").with_provider_name("Days")
                    ],
                }))).required().with_description("Specifies a list of S3 Intelligent-Tiering storage class tiers in the configuration. At least one tier must be defined in the list. At most, you can s...").with_provider_name("Tierings")
                    ],
                })))
                .with_description("Defines how Amazon S3 handles Intelligent-Tiering storage.")
                .with_provider_name("IntelligentTieringConfigurations"),
        )
        .attribute(
            AttributeSchema::new("inventory_configurations", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "InventoryConfiguration".to_string(),
                    fields: vec![
                    StructField::new("destination", AttributeType::Struct {
                    name: "Destination".to_string(),
                    fields: vec![
                    StructField::new("bucket_account_id", AttributeType::String).with_description("The account ID that owns the destination S3 bucket. If no account ID is provided, the owner is not validated before exporting data.  Although this val...").with_provider_name("BucketAccountId"),
                    StructField::new("bucket_arn", super::arn()).required().with_description("The Amazon Resource Name (ARN) of the bucket to which data is exported.").with_provider_name("BucketArn"),
                    StructField::new("format", AttributeType::Enum(vec!["CSV".to_string(), "ORC".to_string(), "Parquet".to_string()])).required().with_description("Specifies the file format used when exporting data to Amazon S3. *Allowed values*: ``CSV`` | ``ORC`` | ``Parquet``").with_provider_name("Format"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix to use when exporting data. The prefix is prepended to all results.").with_provider_name("Prefix")
                    ],
                }).required().with_description("Contains information about where to publish the inventory results.").with_provider_name("Destination"),
                    StructField::new("enabled", AttributeType::Bool).required().with_description("Specifies whether the inventory is enabled or disabled. If set to ``True``, an inventory list is generated. If set to ``False``, no inventory list is ...").with_provider_name("Enabled"),
                    StructField::new("id", AttributeType::String).required().with_description("The ID used to identify the inventory configuration.").with_provider_name("Id"),
                    StructField::new("included_object_versions", AttributeType::Enum(vec!["All".to_string(), "Current".to_string()])).required().with_description("Object versions to include in the inventory list. If set to ``All``, the list includes all the object versions, which adds the version-related fields ...").with_provider_name("IncludedObjectVersions"),
                    StructField::new("optional_fields", AttributeType::List(Box::new(AttributeType::String))).with_description("Contains the optional fields that are included in the inventory results.").with_provider_name("OptionalFields"),
                    StructField::new("prefix", AttributeType::String).with_description("Specifies the inventory filter prefix.").with_provider_name("Prefix"),
                    StructField::new("schedule_frequency", AttributeType::Enum(vec!["Daily".to_string(), "Weekly".to_string()])).required().with_description("Specifies the schedule for generating inventory results.").with_provider_name("ScheduleFrequency")
                    ],
                })))
                .with_description("Specifies the S3 Inventory configuration for an Amazon S3 bucket. For more information, see [GET Bucket inventory](https://docs.aws.amazon.com/AmazonS...")
                .with_provider_name("InventoryConfigurations"),
        )
        .attribute(
            AttributeSchema::new("lifecycle_configuration", AttributeType::Struct {
                    name: "LifecycleConfiguration".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Rule".to_string(),
                    fields: vec![
                    StructField::new("abort_incomplete_multipart_upload", AttributeType::Struct {
                    name: "AbortIncompleteMultipartUpload".to_string(),
                    fields: vec![
                    StructField::new("days_after_initiation", AttributeType::Int).required().with_description("Specifies the number of days after which Amazon S3 stops an incomplete multipart upload.").with_provider_name("DaysAfterInitiation")
                    ],
                }).with_description("Specifies a lifecycle rule that stops incomplete multipart uploads to an Amazon S3 bucket.").with_provider_name("AbortIncompleteMultipartUpload"),
                    StructField::new("expiration_date", AttributeType::String).with_description("Indicates when objects are deleted from Amazon S3 and Amazon S3 Glacier. The date value must be in ISO 8601 format. The time is always midnight UTC. I...").with_provider_name("ExpirationDate"),
                    StructField::new("expiration_in_days", AttributeType::Int).with_description("Indicates the number of days after creation when objects are deleted from Amazon S3 and Amazon S3 Glacier. If you specify an expiration and transition...").with_provider_name("ExpirationInDays"),
                    StructField::new("expired_object_delete_marker", AttributeType::Bool).with_description("Indicates whether Amazon S3 will remove a delete marker without any noncurrent versions. If set to true, the delete marker will be removed if there ar...").with_provider_name("ExpiredObjectDeleteMarker"),
                    StructField::new("id", AttributeType::String).with_description("Unique identifier for the rule. The value can't be longer than 255 characters.").with_provider_name("Id"),
                    StructField::new("noncurrent_version_expiration", AttributeType::Struct {
                    name: "NoncurrentVersionExpiration".to_string(),
                    fields: vec![
                    StructField::new("newer_noncurrent_versions", AttributeType::Int).with_description("Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent versions, S3 will take the associated action. For...").with_provider_name("NewerNoncurrentVersions"),
                    StructField::new("noncurrent_days", AttributeType::Int).required().with_description("Specifies the number of days an object is noncurrent before S3 can perform the associated action. For information about the noncurrent days calculatio...").with_provider_name("NoncurrentDays")
                    ],
                }).with_description("Specifies when noncurrent object versions expire. Upon expiration, S3 permanently deletes the noncurrent object versions. You set this lifecycle confi...").with_provider_name("NoncurrentVersionExpiration"),
                    StructField::new("noncurrent_version_expiration_in_days", AttributeType::Int).with_description("(Deprecated.) For buckets with versioning enabled (or suspended), specifies the time, in days, between when a new version of the object is uploaded to...").with_provider_name("NoncurrentVersionExpirationInDays"),
                    StructField::new("noncurrent_version_transition", AttributeType::Struct {
                    name: "NoncurrentVersionTransition".to_string(),
                    fields: vec![
                    StructField::new("newer_noncurrent_versions", AttributeType::Int).with_description("Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent versions, S3 will take the associated action. For...").with_provider_name("NewerNoncurrentVersions"),
                    StructField::new("storage_class", AttributeType::Enum(vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "Glacier".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()])).required().with_description("The class of storage used to store the object.").with_provider_name("StorageClass"),
                    StructField::new("transition_in_days", AttributeType::Int).required().with_description("Specifies the number of days an object is noncurrent before Amazon S3 can perform the associated action. For information about the noncurrent days cal...").with_provider_name("TransitionInDays")
                    ],
                }).with_description("(Deprecated.) For buckets with versioning enabled (or suspended), specifies when non-current objects transition to a specified storage class. If you s...").with_provider_name("NoncurrentVersionTransition"),
                    StructField::new("noncurrent_version_transitions", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "NoncurrentVersionTransition".to_string(),
                    fields: vec![
                    StructField::new("newer_noncurrent_versions", AttributeType::Int).with_description("Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent versions, S3 will take the associated action. For...").with_provider_name("NewerNoncurrentVersions"),
                    StructField::new("storage_class", AttributeType::Enum(vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "Glacier".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()])).required().with_description("The class of storage used to store the object.").with_provider_name("StorageClass"),
                    StructField::new("transition_in_days", AttributeType::Int).required().with_description("Specifies the number of days an object is noncurrent before Amazon S3 can perform the associated action. For information about the noncurrent days cal...").with_provider_name("TransitionInDays")
                    ],
                }))).with_description("For buckets with versioning enabled (or suspended), one or more transition rules that specify when non-current objects transition to a specified stora...").with_provider_name("NoncurrentVersionTransitions"),
                    StructField::new("object_size_greater_than", AttributeType::String).with_description("Specifies the minimum object size in bytes for this rule to apply to. Objects must be larger than this value in bytes. For more information about size...").with_provider_name("ObjectSizeGreaterThan"),
                    StructField::new("object_size_less_than", AttributeType::String).with_description("Specifies the maximum object size in bytes for this rule to apply to. Objects must be smaller than this value in bytes. For more information about siz...").with_provider_name("ObjectSizeLessThan"),
                    StructField::new("prefix", AttributeType::String).with_description("Object key prefix that identifies one or more objects to which this rule applies.  Replacement must be made for object keys containing special charact...").with_provider_name("Prefix"),
                    StructField::new("status", AttributeType::Enum(vec!["Enabled".to_string(), "Disabled".to_string()])).required().with_description("If ``Enabled``, the rule is currently being applied. If ``Disabled``, the rule is not currently being applied.").with_provider_name("Status"),
                    StructField::new("tag_filters", AttributeType::List(Box::new(tags_type()))).with_description("Tags to use to identify a subset of objects to which the lifecycle rule applies.").with_provider_name("TagFilters"),
                    StructField::new("transition", AttributeType::Struct {
                    name: "Transition".to_string(),
                    fields: vec![
                    StructField::new("storage_class", AttributeType::Enum(vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "Glacier".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()])).required().with_description("The storage class to which you want the object to transition.").with_provider_name("StorageClass"),
                    StructField::new("transition_date", AttributeType::String).with_description("Indicates when objects are transitioned to the specified storage class. The date value must be in ISO 8601 format. The time is always midnight UTC.").with_provider_name("TransitionDate"),
                    StructField::new("transition_in_days", AttributeType::Int).with_description("Indicates the number of days after creation when objects are transitioned to the specified storage class. If the specified storage class is ``INTELLIG...").with_provider_name("TransitionInDays")
                    ],
                }).with_description("(Deprecated.) Specifies when an object transitions to a specified storage class. If you specify an expiration and transition time, you must use the sa...").with_provider_name("Transition"),
                    StructField::new("transitions", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Transition".to_string(),
                    fields: vec![
                    StructField::new("storage_class", AttributeType::Enum(vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "Glacier".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "STANDARD_IA".to_string()])).required().with_description("The storage class to which you want the object to transition.").with_provider_name("StorageClass"),
                    StructField::new("transition_date", AttributeType::String).with_description("Indicates when objects are transitioned to the specified storage class. The date value must be in ISO 8601 format. The time is always midnight UTC.").with_provider_name("TransitionDate"),
                    StructField::new("transition_in_days", AttributeType::Int).with_description("Indicates the number of days after creation when objects are transitioned to the specified storage class. If the specified storage class is ``INTELLIG...").with_provider_name("TransitionInDays")
                    ],
                }))).with_description("One or more transition rules that specify when an object transitions to a specified storage class. If you specify an expiration and transition time, y...").with_provider_name("Transitions")
                    ],
                }))).required().with_description("A lifecycle rule for individual objects in an Amazon S3 bucket.").with_provider_name("Rules"),
                    StructField::new("transition_default_minimum_object_size", AttributeType::Enum(vec!["varies_by_storage_class".to_string(), "all_storage_classes_128K".to_string()])).with_description("Indicates which default minimum object size behavior is applied to the lifecycle configuration.  This parameter applies to general purpose buckets onl...").with_provider_name("TransitionDefaultMinimumObjectSize")
                    ],
                })
                .with_description("Specifies the lifecycle configuration for objects in an Amazon S3 bucket. For more information, see [Object Lifecycle Management](https://docs.aws.ama...")
                .with_provider_name("LifecycleConfiguration"),
        )
        .attribute(
            AttributeSchema::new("logging_configuration", AttributeType::Struct {
                    name: "LoggingConfiguration".to_string(),
                    fields: vec![
                    StructField::new("destination_bucket_name", AttributeType::String).with_description("The name of the bucket where Amazon S3 should store server access log files. You can store log files in any bucket that you own. By default, logs are ...").with_provider_name("DestinationBucketName"),
                    StructField::new("log_file_prefix", AttributeType::String).with_description("A prefix for all log object keys. If you store log files from multiple Amazon S3 buckets in a single bucket, you can use a prefix to distinguish which...").with_provider_name("LogFilePrefix"),
                    StructField::new("target_object_key_format", AttributeType::String).with_description("Amazon S3 key format for log objects. Only one format, either PartitionedPrefix or SimplePrefix, is allowed.").with_provider_name("TargetObjectKeyFormat")
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
                    StructField::new("table_bucket_type", AttributeType::Enum(vec!["aws".to_string(), "customer".to_string()])).required().with_description("The type of the table bucket where the metadata configuration is stored. The ``aws`` value indicates an AWS managed table bucket, and the ``customer``...").with_provider_name("TableBucketType"),
                    StructField::new("table_namespace", AttributeType::String).with_description("The namespace in the table bucket where the metadata tables for a metadata configuration are stored.").with_provider_name("TableNamespace")
                    ],
                }).with_description("The destination information for the S3 Metadata configuration.").with_provider_name("Destination"),
                    StructField::new("inventory_table_configuration", AttributeType::Struct {
                    name: "InventoryTableConfiguration".to_string(),
                    fields: vec![
                    StructField::new("configuration_state", AttributeType::Enum(vec!["ENABLED".to_string(), "DISABLED".to_string()])).required().with_description("The configuration state of the inventory table, indicating whether the inventory table is enabled or disabled.").with_provider_name("ConfigurationState"),
                    StructField::new("encryption_configuration", AttributeType::Struct {
                    name: "MetadataTableEncryptionConfiguration".to_string(),
                    fields: vec![
                    StructField::new("kms_key_arn", super::kms_key_arn()).with_description("If server-side encryption with KMSlong (KMS) keys (SSE-KMS) is specified, you must also specify the KMS key Amazon Resource Name (ARN). You must speci...").with_provider_name("KmsKeyArn"),
                    StructField::new("sse_algorithm", AttributeType::Enum(vec!["aws:kms".to_string(), "AES256".to_string()])).required().with_description("The encryption type specified for a metadata table. To specify server-side encryption with KMSlong (KMS) keys (SSE-KMS), use the ``aws:kms`` value. To...").with_provider_name("SseAlgorithm")
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
                    StructField::new("kms_key_arn", super::kms_key_arn()).with_description("If server-side encryption with KMSlong (KMS) keys (SSE-KMS) is specified, you must also specify the KMS key Amazon Resource Name (ARN). You must speci...").with_provider_name("KmsKeyArn"),
                    StructField::new("sse_algorithm", AttributeType::Enum(vec!["aws:kms".to_string(), "AES256".to_string()])).required().with_description("The encryption type specified for a metadata table. To specify server-side encryption with KMSlong (KMS) keys (SSE-KMS), use the ``aws:kms`` value. To...").with_provider_name("SseAlgorithm")
                    ],
                }).with_description("The encryption configuration for the journal table.").with_provider_name("EncryptionConfiguration"),
                    StructField::new("record_expiration", AttributeType::Struct {
                    name: "RecordExpiration".to_string(),
                    fields: vec![
                    StructField::new("days", AttributeType::Int).with_description("If you enable journal table record expiration, you can set the number of days to retain your journal table records. Journal table records must be reta...").with_provider_name("Days"),
                    StructField::new("expiration", AttributeType::Enum(vec!["ENABLED".to_string(), "DISABLED".to_string()])).required().with_description("Specifies whether journal table record expiration is enabled or disabled.").with_provider_name("Expiration")
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
                    StructField::new("table_arn", super::arn()).with_description("The Amazon Resource Name (ARN) for the metadata table in the metadata table configuration. The specified metadata table name must be unique within the...").with_provider_name("TableArn"),
                    StructField::new("table_bucket_arn", super::arn()).required().with_description("The Amazon Resource Name (ARN) for the table bucket that's specified as the destination in the metadata table configuration. The destination table buc...").with_provider_name("TableBucketArn"),
                    StructField::new("table_name", AttributeType::String).required().with_description("The name for the metadata table in your metadata table configuration. The specified metadata table name must be unique within the ``aws_s3_metadata`` ...").with_provider_name("TableName"),
                    StructField::new("table_namespace", AttributeType::String).with_description("The table bucket namespace for the metadata table in your metadata table configuration. This value is always ``aws_s3_metadata``.").with_provider_name("TableNamespace")
                    ],
                }).required().with_description("The destination information for the metadata table configuration. The destination table bucket must be in the same Region and AWS-account as the gener...").with_provider_name("S3TablesDestination")
                    ],
                })
                .with_description("The metadata table configuration of an S3 general purpose bucket.")
                .with_provider_name("MetadataTableConfiguration"),
        )
        .attribute(
            AttributeSchema::new("metrics_configurations", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "MetricsConfiguration".to_string(),
                    fields: vec![
                    StructField::new("access_point_arn", super::arn()).with_description("The access point that was used while performing operations on the object. The metrics configuration only includes objects that meet the filter's crite...").with_provider_name("AccessPointArn"),
                    StructField::new("id", AttributeType::String).required().with_description("The ID used to identify the metrics configuration. This can be any value you choose that helps you identify your metrics configuration.").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::String).with_description("The prefix that an object must have to be included in the metrics results.").with_provider_name("Prefix"),
                    StructField::new("tag_filters", AttributeType::List(Box::new(tags_type()))).with_description("Specifies a list of tag filters to use as a metrics configuration filter. The metrics configuration includes only objects that meet the filter's crite...").with_provider_name("TagFilters")
                    ],
                })))
                .with_description("Specifies a metrics configuration for the CloudWatch request metrics (specified by the metrics configuration ID) from an Amazon S3 bucket. If you're u...")
                .with_provider_name("MetricsConfigurations"),
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
                    StructField::new("lambda_configurations", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "LambdaConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event", AttributeType::String).required().with_description("The Amazon S3 bucket event for which to invoke the LAMlong function. For more information, see [Supported Event Types](https://docs.aws.amazon.com/Ama...").with_provider_name("Event"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "NotificationFilter".to_string(),
                    fields: vec![
                    StructField::new("s3_key", AttributeType::Struct {
                    name: "S3KeyFilter".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "FilterRule".to_string(),
                    fields: vec![
                    StructField::new("name", AttributeType::String).required().with_description("The object key name prefix or suffix identifying one or more objects to which the filtering rule applies. The maximum length is 1,024 characters. Over...").with_provider_name("Name"),
                    StructField::new("value", AttributeType::String).required().with_description("The value that the filter searches for in object key names.").with_provider_name("Value")
                    ],
                }))).required().with_description("A list of containers for the key-value pair that defines the criteria for the filter rule.").with_provider_name("Rules")
                    ],
                }).required().with_description("A container for object key name prefix and suffix filtering rules.").with_provider_name("S3Key")
                    ],
                }).with_description("The filtering rules that determine which objects invoke the AWS Lambda function. For example, you can create a filter so that only image files with a ...").with_provider_name("Filter"),
                    StructField::new("function", AttributeType::String).required().with_description("The Amazon Resource Name (ARN) of the LAMlong function that Amazon S3 invokes when the specified event type occurs.").with_provider_name("Function")
                    ],
                }))).with_description("Describes the LAMlong functions to invoke and the events for which to invoke them.").with_provider_name("LambdaConfigurations"),
                    StructField::new("queue_configurations", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "QueueConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event", AttributeType::String).required().with_description("The Amazon S3 bucket event about which you want to publish messages to Amazon SQS. For more information, see [Supported Event Types](https://docs.aws....").with_provider_name("Event"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "NotificationFilter".to_string(),
                    fields: vec![
                    StructField::new("s3_key", AttributeType::Struct {
                    name: "S3KeyFilter".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "FilterRule".to_string(),
                    fields: vec![
                    StructField::new("name", AttributeType::String).required().with_description("The object key name prefix or suffix identifying one or more objects to which the filtering rule applies. The maximum length is 1,024 characters. Over...").with_provider_name("Name"),
                    StructField::new("value", AttributeType::String).required().with_description("The value that the filter searches for in object key names.").with_provider_name("Value")
                    ],
                }))).required().with_description("A list of containers for the key-value pair that defines the criteria for the filter rule.").with_provider_name("Rules")
                    ],
                }).required().with_description("A container for object key name prefix and suffix filtering rules.").with_provider_name("S3Key")
                    ],
                }).with_description("The filtering rules that determine which objects trigger notifications. For example, you can create a filter so that Amazon S3 sends notifications onl...").with_provider_name("Filter"),
                    StructField::new("queue", AttributeType::String).required().with_description("The Amazon Resource Name (ARN) of the Amazon SQS queue to which Amazon S3 publishes a message when it detects events of the specified type. FIFO queue...").with_provider_name("Queue")
                    ],
                }))).with_description("The Amazon Simple Queue Service queues to publish messages to and the events for which to publish messages.").with_provider_name("QueueConfigurations"),
                    StructField::new("topic_configurations", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "TopicConfiguration".to_string(),
                    fields: vec![
                    StructField::new("event", AttributeType::String).required().with_description("The Amazon S3 bucket event about which to send notifications. For more information, see [Supported Event Types](https://docs.aws.amazon.com/AmazonS3/l...").with_provider_name("Event"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "NotificationFilter".to_string(),
                    fields: vec![
                    StructField::new("s3_key", AttributeType::Struct {
                    name: "S3KeyFilter".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "FilterRule".to_string(),
                    fields: vec![
                    StructField::new("name", AttributeType::String).required().with_description("The object key name prefix or suffix identifying one or more objects to which the filtering rule applies. The maximum length is 1,024 characters. Over...").with_provider_name("Name"),
                    StructField::new("value", AttributeType::String).required().with_description("The value that the filter searches for in object key names.").with_provider_name("Value")
                    ],
                }))).required().with_description("A list of containers for the key-value pair that defines the criteria for the filter rule.").with_provider_name("Rules")
                    ],
                }).required().with_description("A container for object key name prefix and suffix filtering rules.").with_provider_name("S3Key")
                    ],
                }).with_description("The filtering rules that determine for which objects to send notifications. For example, you can create a filter so that Amazon S3 sends notifications...").with_provider_name("Filter"),
                    StructField::new("topic", AttributeType::String).required().with_description("The Amazon Resource Name (ARN) of the Amazon SNS topic to which Amazon S3 publishes a message when it detects events of the specified type.").with_provider_name("Topic")
                    ],
                }))).with_description("The topic to which notifications are sent and the events for which notifications are generated.").with_provider_name("TopicConfigurations")
                    ],
                })
                .with_description("Configuration that defines how Amazon S3 handles bucket notifications.")
                .with_provider_name("NotificationConfiguration"),
        )
        .attribute(
            AttributeSchema::new("object_lock_configuration", AttributeType::Struct {
                    name: "ObjectLockConfiguration".to_string(),
                    fields: vec![
                    StructField::new("object_lock_enabled", AttributeType::String).with_description("Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled`` when you apply ``ObjectLockConfiguration`` to a b...").with_provider_name("ObjectLockEnabled"),
                    StructField::new("rule", AttributeType::Struct {
                    name: "ObjectLockRule".to_string(),
                    fields: vec![
                    StructField::new("default_retention", AttributeType::Struct {
                    name: "DefaultRetention".to_string(),
                    fields: vec![
                    StructField::new("days", AttributeType::Int).with_description("The number of days that you want to specify for the default retention period. If Object Lock is turned on, you must specify ``Mode`` and specify eithe...").with_provider_name("Days"),
                    StructField::new("mode", AttributeType::Enum(vec!["COMPLIANCE".to_string(), "GOVERNANCE".to_string()])).with_description("The default Object Lock retention mode you want to apply to new objects placed in the specified bucket. If Object Lock is turned on, you must specify ...").with_provider_name("Mode"),
                    StructField::new("years", AttributeType::Int).with_description("The number of years that you want to specify for the default retention period. If Object Lock is turned on, you must specify ``Mode`` and specify eith...").with_provider_name("Years")
                    ],
                }).with_description("The default Object Lock retention mode and period that you want to apply to new objects placed in the specified bucket. If Object Lock is turned on, b...").with_provider_name("DefaultRetention")
                    ],
                }).with_description("Specifies the Object Lock rule for the specified object. Enable this rule when you apply ``ObjectLockConfiguration`` to a bucket. If Object Lock is tu...").with_provider_name("Rule")
                    ],
                })
                .with_description("This operation is not supported for directory buckets.  Places an Object Lock configuration on the specified bucket. The rule specified in the Object ...")
                .with_provider_name("ObjectLockConfiguration"),
        )
        .attribute(
            AttributeSchema::new("object_lock_enabled", AttributeType::Bool)
                .with_description("Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled`` when you apply ``ObjectLockConfiguration`` to a b...")
                .with_provider_name("ObjectLockEnabled"),
        )
        .attribute(
            AttributeSchema::new("ownership_controls", AttributeType::Struct {
                    name: "OwnershipControls".to_string(),
                    fields: vec![
                    StructField::new("rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "OwnershipControlsRule".to_string(),
                    fields: vec![
                    StructField::new("object_ownership", AttributeType::Enum(vec!["ObjectWriter".to_string(), "BucketOwnerPreferred".to_string(), "BucketOwnerEnforced".to_string()])).with_description("Specifies an object ownership rule.").with_provider_name("ObjectOwnership")
                    ],
                }))).required().with_description("Specifies the container element for Object Ownership rules.").with_provider_name("Rules")
                    ],
                })
                .with_description("Configuration that defines how Amazon S3 handles Object Ownership rules.")
                .with_provider_name("OwnershipControls"),
        )
        .attribute(
            AttributeSchema::new("public_access_block_configuration", AttributeType::Struct {
                    name: "PublicAccessBlockConfiguration".to_string(),
                    fields: vec![
                    StructField::new("block_public_acls", AttributeType::Bool).with_description("Specifies whether Amazon S3 should block public access control lists (ACLs) for this bucket and objects in this bucket. Setting this element to ``TRUE...").with_provider_name("BlockPublicAcls"),
                    StructField::new("block_public_policy", AttributeType::Bool).with_description("Specifies whether Amazon S3 should block public bucket policies for this bucket. Setting this element to ``TRUE`` causes Amazon S3 to reject calls to ...").with_provider_name("BlockPublicPolicy"),
                    StructField::new("ignore_public_acls", AttributeType::Bool).with_description("Specifies whether Amazon S3 should ignore public ACLs for this bucket and objects in this bucket. Setting this element to ``TRUE`` causes Amazon S3 to...").with_provider_name("IgnorePublicAcls"),
                    StructField::new("restrict_public_buckets", AttributeType::Bool).with_description("Specifies whether Amazon S3 should restrict public bucket policies for this bucket. Setting this element to ``TRUE`` restricts access to this bucket t...").with_provider_name("RestrictPublicBuckets")
                    ],
                })
                .with_description("Configuration that defines how Amazon S3 handles public access.")
                .with_provider_name("PublicAccessBlockConfiguration"),
        )
        .attribute(
            AttributeSchema::new("regional_domain_name", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("RegionalDomainName"),
        )
        .attribute(
            AttributeSchema::new("replication_configuration", AttributeType::Struct {
                    name: "ReplicationConfiguration".to_string(),
                    fields: vec![
                    StructField::new("role", AttributeType::String).required().with_description("The Amazon Resource Name (ARN) of the IAMlong (IAM) role that Amazon S3 assumes when replicating objects. For more information, see [How to Set Up Rep...").with_provider_name("Role"),
                    StructField::new("rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "ReplicationRule".to_string(),
                    fields: vec![
                    StructField::new("delete_marker_replication", AttributeType::Struct {
                    name: "DeleteMarkerReplication".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::Enum(vec!["Disabled".to_string(), "Enabled".to_string()])).with_description("Indicates whether to replicate delete markers.").with_provider_name("Status")
                    ],
                }).with_description("Specifies whether Amazon S3 replicates delete markers. If you specify a ``Filter`` in your replication configuration, you must also include a ``Delete...").with_provider_name("DeleteMarkerReplication"),
                    StructField::new("destination", AttributeType::Struct {
                    name: "ReplicationDestination".to_string(),
                    fields: vec![
                    StructField::new("access_control_translation", AttributeType::Struct {
                    name: "AccessControlTranslation".to_string(),
                    fields: vec![
                    StructField::new("owner", AttributeType::String).required().with_description("Specifies the replica ownership. For default and valid values, see [PUT bucket replication](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucket...").with_provider_name("Owner")
                    ],
                }).with_description("Specify this only in a cross-account scenario (where source and destination bucket owners are not the same), and you want to change replica ownership ...").with_provider_name("AccessControlTranslation"),
                    StructField::new("account", AttributeType::String).with_description("Destination bucket owner account ID. In a cross-account scenario, if you direct Amazon S3 to change replica ownership to the AWS-account that owns the...").with_provider_name("Account"),
                    StructField::new("bucket", AttributeType::String).required().with_description("The Amazon Resource Name (ARN) of the bucket where you want Amazon S3 to store the results.").with_provider_name("Bucket"),
                    StructField::new("encryption_configuration", AttributeType::Struct {
                    name: "EncryptionConfiguration".to_string(),
                    fields: vec![
                    StructField::new("replica_kms_key_id", super::kms_key_arn()).required().with_description("Specifies the ID (Key ARN or Alias ARN) of the customer managed AWS KMS key stored in AWS Key Management Service (KMS) for the destination bucket. Ama...").with_provider_name("ReplicaKmsKeyID")
                    ],
                }).with_description("Specifies encryption-related information.").with_provider_name("EncryptionConfiguration"),
                    StructField::new("metrics", AttributeType::Struct {
                    name: "Metrics".to_string(),
                    fields: vec![
                    StructField::new("event_threshold", AttributeType::Struct {
                    name: "ReplicationTimeValue".to_string(),
                    fields: vec![
                    StructField::new("minutes", AttributeType::Int).required().with_description("Contains an integer specifying time in minutes.  Valid value: 15").with_provider_name("Minutes")
                    ],
                }).with_description("A container specifying the time threshold for emitting the ``s3:Replication:OperationMissedThreshold`` event.").with_provider_name("EventThreshold"),
                    StructField::new("status", AttributeType::Enum(vec!["Disabled".to_string(), "Enabled".to_string()])).required().with_description("Specifies whether the replication metrics are enabled.").with_provider_name("Status")
                    ],
                }).with_description("A container specifying replication metrics-related settings enabling replication metrics and events.").with_provider_name("Metrics"),
                    StructField::new("replication_time", AttributeType::Struct {
                    name: "ReplicationTime".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::Enum(vec!["Disabled".to_string(), "Enabled".to_string()])).required().with_description("Specifies whether the replication time is enabled.").with_provider_name("Status"),
                    StructField::new("time", AttributeType::Struct {
                    name: "ReplicationTimeValue".to_string(),
                    fields: vec![
                    StructField::new("minutes", AttributeType::Int).required().with_description("Contains an integer specifying time in minutes.  Valid value: 15").with_provider_name("Minutes")
                    ],
                }).required().with_description("A container specifying the time by which replication should be complete for all objects and operations on objects.").with_provider_name("Time")
                    ],
                }).with_description("A container specifying S3 Replication Time Control (S3 RTC), including whether S3 RTC is enabled and the time when all objects and operations on objec...").with_provider_name("ReplicationTime"),
                    StructField::new("storage_class", AttributeType::Enum(vec!["DEEP_ARCHIVE".to_string(), "GLACIER".to_string(), "GLACIER_IR".to_string(), "INTELLIGENT_TIERING".to_string(), "ONEZONE_IA".to_string(), "REDUCED_REDUNDANCY".to_string(), "STANDARD".to_string(), "STANDARD_IA".to_string()])).with_description("The storage class to use when replicating objects, such as S3 Standard or reduced redundancy. By default, Amazon S3 uses the storage class of the sour...").with_provider_name("StorageClass")
                    ],
                }).required().with_description("A container for information about the replication destination and its configurations including enabling the S3 Replication Time Control (S3 RTC).").with_provider_name("Destination"),
                    StructField::new("filter", AttributeType::Struct {
                    name: "ReplicationRuleFilter".to_string(),
                    fields: vec![
                    StructField::new("and", AttributeType::Struct {
                    name: "ReplicationRuleAndOperator".to_string(),
                    fields: vec![
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the subset of objects to which the rule applies.").with_provider_name("Prefix"),
                    StructField::new("tag_filters", AttributeType::List(Box::new(tags_type()))).with_description("An array of tags containing key and value pairs.").with_provider_name("TagFilters")
                    ],
                }).with_description("A container for specifying rule filters. The filters determine the subset of objects to which the rule applies. This element is required only if you s...").with_provider_name("And"),
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the subset of objects to which the rule applies.  Replacement must be made for object keys containing specia...").with_provider_name("Prefix"),
                    StructField::new("tag_filter", tags_type()).with_description("A container for specifying a tag key and value.  The rule applies only to objects that have the tag in their tag set.").with_provider_name("TagFilter")
                    ],
                }).with_description("A filter that identifies the subset of objects to which the replication rule applies. A ``Filter`` must specify exactly one ``Prefix``, ``TagFilter``,...").with_provider_name("Filter"),
                    StructField::new("id", AttributeType::String).with_description("A unique identifier for the rule. The maximum value is 255 characters. If you don't specify a value, AWS CloudFormation generates a random ID. When us...").with_provider_name("Id"),
                    StructField::new("prefix", AttributeType::String).with_description("An object key name prefix that identifies the object or objects to which the rule applies. The maximum prefix length is 1,024 characters. To include a...").with_provider_name("Prefix"),
                    StructField::new("priority", AttributeType::Int).with_description("The priority indicates which rule has precedence whenever two or more replication rules conflict. Amazon S3 will attempt to replicate objects accordin...").with_provider_name("Priority"),
                    StructField::new("source_selection_criteria", AttributeType::Struct {
                    name: "SourceSelectionCriteria".to_string(),
                    fields: vec![
                    StructField::new("replica_modifications", AttributeType::Struct {
                    name: "ReplicaModifications".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::Enum(vec!["Enabled".to_string(), "Disabled".to_string()])).required().with_description("Specifies whether Amazon S3 replicates modifications on replicas. *Allowed values*: ``Enabled`` | ``Disabled``").with_provider_name("Status")
                    ],
                }).with_description("A filter that you can specify for selection for modifications on replicas.").with_provider_name("ReplicaModifications"),
                    StructField::new("sse_kms_encrypted_objects", AttributeType::Struct {
                    name: "SseKmsEncryptedObjects".to_string(),
                    fields: vec![
                    StructField::new("status", AttributeType::Enum(vec!["Disabled".to_string(), "Enabled".to_string()])).required().with_description("Specifies whether Amazon S3 replicates objects created with server-side encryption using an AWS KMS key stored in AWS Key Management Service.").with_provider_name("Status")
                    ],
                }).with_description("A container for filter information for the selection of Amazon S3 objects encrypted with AWS KMS.").with_provider_name("SseKmsEncryptedObjects")
                    ],
                }).with_description("A container that describes additional filters for identifying the source objects that you want to replicate. You can choose to enable or disable the r...").with_provider_name("SourceSelectionCriteria"),
                    StructField::new("status", AttributeType::Enum(vec!["Disabled".to_string(), "Enabled".to_string()])).required().with_description("Specifies whether the rule is enabled.").with_provider_name("Status")
                    ],
                }))).required().with_description("A container for one or more replication rules. A replication configuration must have at least one rule and can contain a maximum of 1,000 rules.").with_provider_name("Rules")
                    ],
                })
                .with_description("Configuration for replicating objects in an S3 bucket. To enable replication, you must also enable versioning by using the ``VersioningConfiguration``...")
                .with_provider_name("ReplicationConfiguration"),
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
                    StructField::new("status", AttributeType::Enum(vec!["Enabled".to_string(), "Suspended".to_string()])).required().with_description("The versioning state of the bucket.").with_provider_name("Status")
                    ],
                })
                .with_description("Enables multiple versions of all objects in this bucket. You might enable versioning to prevent objects from being deleted or overwritten by mistake o...")
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
                    StructField::new("protocol", AttributeType::Enum(vec!["http".to_string(), "https".to_string()])).with_description("Protocol to use when redirecting requests. The default is the protocol that is used in the original request.").with_provider_name("Protocol")
                    ],
                }).with_description("The redirect behavior for every request to this bucket's website endpoint.  If you specify this property, you can't specify any other property.").with_provider_name("RedirectAllRequestsTo"),
                    StructField::new("routing_rules", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "RoutingRule".to_string(),
                    fields: vec![
                    StructField::new("redirect_rule", AttributeType::Struct {
                    name: "RedirectRule".to_string(),
                    fields: vec![
                    StructField::new("host_name", AttributeType::String).with_description("The host name to use in the redirect request.").with_provider_name("HostName"),
                    StructField::new("http_redirect_code", AttributeType::String).with_description("The HTTP redirect code to use on the response. Not required if one of the siblings is present.").with_provider_name("HttpRedirectCode"),
                    StructField::new("protocol", AttributeType::Enum(vec!["http".to_string(), "https".to_string()])).with_description("Protocol to use when redirecting requests. The default is the protocol that is used in the original request.").with_provider_name("Protocol"),
                    StructField::new("replace_key_prefix_with", AttributeType::Enum(vec!["docs/".to_string(), "documents/".to_string(), "/documents".to_string()])).with_description("The object key prefix to use in the redirect request. For example, to redirect requests for all pages with prefix ``docs/`` (objects in the ``docs/`` ...").with_provider_name("ReplaceKeyPrefixWith"),
                    StructField::new("replace_key_with", AttributeType::String).with_description("The specific object key to use in the redirect request. For example, redirect request to ``error.html``. Not required if one of the siblings is presen...").with_provider_name("ReplaceKeyWith")
                    ],
                }).required().with_description("Container for redirect information. You can redirect requests to another host, to another page, or with another protocol. In the event of an error, yo...").with_provider_name("RedirectRule"),
                    StructField::new("routing_rule_condition", AttributeType::Struct {
                    name: "RoutingRuleCondition".to_string(),
                    fields: vec![
                    StructField::new("http_error_code_returned_equals", AttributeType::String).with_description("The HTTP error code when the redirect is applied. In the event of an error, if the error code equals this value, then the specified redirect is applie...").with_provider_name("HttpErrorCodeReturnedEquals"),
                    StructField::new("key_prefix_equals", AttributeType::String).with_description("The object key name prefix when the redirect is applied. For example, to redirect requests for ``ExamplePage.html``, the key prefix will be ``ExampleP...").with_provider_name("KeyPrefixEquals")
                    ],
                }).with_description("A container for describing a condition that must be met for the specified redirect to apply. For example, 1. If request is for pages in the ``/docs`` ...").with_provider_name("RoutingRuleCondition")
                    ],
                }))).with_description("Rules that define when a redirect is applied and the redirect behavior.").with_provider_name("RoutingRules")
                    ],
                })
                .with_description("Information used to configure the bucket as a static website. For more information, see [Hosting Websites on Amazon S3](https://docs.aws.amazon.com/Am...")
                .with_provider_name("WebsiteConfiguration"),
        )
        .attribute(
            AttributeSchema::new("website_url", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("WebsiteURL"),
        )
    }
}
