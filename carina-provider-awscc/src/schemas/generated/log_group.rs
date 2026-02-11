//! log_group schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::Logs::LogGroup
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

const VALID_LOG_GROUP_CLASS: &[&str] = &["STANDARD", "INFREQUENT_ACCESS", "DELIVERY"];

fn validate_log_group_class(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "LogGroupClass",
        "awscc.logs_log_group",
        VALID_LOG_GROUP_CLASS,
    )
}

/// Returns the schema config for logs_log_group (AWS::Logs::LogGroup)
pub fn logs_log_group_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::Logs::LogGroup",
        resource_type_name: "logs_log_group",
        has_tags: true,
        schema: ResourceSchema::new("awscc.logs_log_group")
        .with_description("The ``AWS::Logs::LogGroup`` resource specifies a log group. A log group defines common properties for log streams, such as their retention and access control rules. Each log stream must belong to one ...")
        .attribute(
            AttributeSchema::new("arn", super::arn())
                .with_description(" (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("data_protection_policy", AttributeType::Map(Box::new(AttributeType::String)))
                .with_description("Creates a data protection policy and assigns it to the log group. A data protection policy can help safeguard sensitive data that's ingested by the lo...")
                .with_provider_name("DataProtectionPolicy"),
        )
        .attribute(
            AttributeSchema::new("deletion_protection_enabled", AttributeType::Bool)
                .with_description("Indicates whether deletion protection is enabled for this log group. When enabled, deletion protection blocks all deletion operations until it is expl...")
                .with_provider_name("DeletionProtectionEnabled"),
        )
        .attribute(
            AttributeSchema::new("field_index_policies", AttributeType::List(Box::new(AttributeType::Map(Box::new(AttributeType::String)))))
                .with_description("Creates or updates a *field index policy* for the specified log group. Only log groups in the Standard log class support field index policies. For mor...")
                .with_provider_name("FieldIndexPolicies"),
        )
        .attribute(
            AttributeSchema::new("kms_key_id", AttributeType::String)
                .with_description("The Amazon Resource Name (ARN) of the KMS key to use when encrypting log data. To associate an KMS key with the log group, specify the ARN of that KMS...")
                .with_provider_name("KmsKeyId"),
        )
        .attribute(
            AttributeSchema::new("log_group_class", AttributeType::Custom {
                name: "LogGroupClass".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_log_group_class,
                namespace: Some("awscc.logs_log_group".to_string()),
            })
                .with_description("Specifies the log group class for this log group. There are two classes:  + The ``Standard`` log class supports all CWL features.  + The ``Infrequent ...")
                .with_provider_name("LogGroupClass"),
        )
        .attribute(
            AttributeSchema::new("log_group_name", AttributeType::String)
                .with_description("The name of the log group. If you don't specify a name, CFNlong generates a unique ID for the log group.")
                .with_provider_name("LogGroupName"),
        )
        .attribute(
            AttributeSchema::new("resource_policy_document", super::iam_policy_document())
                .with_description("Creates or updates a resource policy for the specified log group that allows other services to put log events to this account. A LogGroup can have 1 r...")
                .with_provider_name("ResourcePolicyDocument"),
        )
        .attribute(
            AttributeSchema::new("retention_in_days", AttributeType::Int)
                .with_description("The number of days to retain the log events in the specified log group. Possible values are: 1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545,...")
                .with_provider_name("RetentionInDays"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("An array of key-value pairs to apply to the log group. For more information, see [Tag](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/...")
                .with_provider_name("Tags"),
        )
    }
}
