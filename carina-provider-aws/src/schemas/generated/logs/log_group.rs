//! log_group schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.cloudwatchlogs
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

const VALID_RETENTION_IN_DAYS_VALUES: &[i64] = &[
    1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922,
    3288, 3653,
];

fn validate_retention_in_days_int_enum(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if VALID_RETENTION_IN_DAYS_VALUES.contains(n) {
            Ok(())
        } else {
            Err(format!(
                "Value {} is not a valid retention_in_days value",
                n
            ))
        }
    } else {
        Err("Expected integer".to_string())
    }
}

const VALID_LOG_GROUP_CLASS: &[&str] = &["STANDARD", "INFREQUENT_ACCESS"];

/// Returns the schema config for logs.log_group (Smithy: com.amazonaws.cloudwatchlogs)
pub fn logs_log_group_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::Logs::LogGroup",
        resource_type_name: "logs.log_group",
        has_tags: true,
        schema: ResourceSchema::new("aws.logs.log_group")
            .with_description("A CloudWatch Logs log group.")
            .attribute(
                AttributeSchema::new("arn", super::arn())
                    .read_only()
                    .with_description("The ARN of the log group. (read-only)")
                    .with_provider_name("Arn"),
            )
            .attribute(
                AttributeSchema::new("log_group_class", AttributeType::StringEnum {
                    name: "LogGroupClass".to_string(),
                    values: vec!["STANDARD".to_string(), "INFREQUENT_ACCESS".to_string()],
                    namespace: Some("aws.logs.log_group".to_string()),
                    to_dsl: None,
                })
                    .create_only()
                    .with_description("The log group class. STANDARD or INFREQUENT_ACCESS.")
                    .with_provider_name("LogGroupClass")
                    .with_default(Value::String("STANDARD".to_string())),
            )
            .attribute(
                AttributeSchema::new("log_group_name", AttributeType::String)
                    .create_only()
                    .with_description("The name of the log group.")
                    .with_provider_name("LogGroupName"),
            )
            .attribute(
                AttributeSchema::new("retention_in_days", AttributeType::Custom {
                    name: "IntEnum([1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653])".to_string(),
                    base: Box::new(AttributeType::Int),
                    validate: validate_retention_in_days_int_enum,
                    namespace: None,
                    to_dsl: None,
                })
                    .with_description("The number of days to retain log events. If omitted, logs never expire.")
                    .with_provider_name("RetentionInDays"),
            )
            .attribute(
                AttributeSchema::new("kms_key_id", super::kms_key_id())
                    .with_description("The ARN of the KMS key to use for encrypting log data.")
                    .with_provider_name("KmsKeyId"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags for the log group.")
                    .with_provider_name("Tags"),
            )
            .with_name_attribute("log_group_name"),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "logs.log_group",
        &[("log_group_class", VALID_LOG_GROUP_CLASS)],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
