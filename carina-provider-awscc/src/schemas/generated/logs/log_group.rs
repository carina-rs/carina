//! log_group schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::Logs::LogGroup
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use regex::Regex;

const VALID_LOG_GROUP_CLASS: &[&str] = &["STANDARD", "INFREQUENT_ACCESS", "DELIVERY"];

const VALID_RETENTION_IN_DAYS_VALUES: &[i64] = &[
    1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922,
    3288, 3653,
];

fn validate_retention_in_days_int_enum(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if VALID_RETENTION_IN_DAYS_VALUES.contains(n) {
            Ok(())
        } else {
            Err(format!("Value {} is not a valid value", n))
        }
    } else {
        Err("Expected integer".to_string())
    }
}

fn validate_string_pattern_b6dfbc56753dfe38_len_1_512(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
            Regex::new("^[.\\-_/#A-Za-z0-9]{1,512}\\z").expect("invalid pattern regex")
        });
        if !RE.is_match(s) {
            return Err(format!(
                "Value '{}' does not match pattern ^[.\\-_/#A-Za-z0-9]{{1,512}}\\z",
                s
            ));
        }
        let len = s.chars().count();
        if !(1..=512).contains(&len) {
            return Err(format!("String length {} is out of range 1..=512", len));
        }
        Ok(())
    } else {
        Err("Expected string".to_string())
    }
}

/// Returns the schema config for logs_log_group (AWS::Logs::LogGroup)
pub fn logs_log_group_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::Logs::LogGroup",
        resource_type_name: "logs.log_group",
        has_tags: true,
        schema: ResourceSchema::new("awscc.logs.log_group")
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
                .with_provider_name("DeletionProtectionEnabled")
                .with_default(Value::Bool(false)),
        )
        .attribute(
            AttributeSchema::new("field_index_policies", AttributeType::List(Box::new(AttributeType::Map(Box::new(AttributeType::String)))))
                .with_description("Creates or updates a *field index policy* for the specified log group. Only log groups in the Standard log class support field index policies. For mor...")
                .with_provider_name("FieldIndexPolicies"),
        )
        .attribute(
            AttributeSchema::new("kms_key_id", super::kms_key_arn())
                .with_description("The Amazon Resource Name (ARN) of the KMS key to use when encrypting log data. To associate an KMS key with the log group, specify the ARN of that KMS...")
                .with_provider_name("KmsKeyId"),
        )
        .attribute(
            AttributeSchema::new("log_group_class", AttributeType::StringEnum {
                name: "LogGroupClass".to_string(),
                values: vec!["STANDARD".to_string(), "INFREQUENT_ACCESS".to_string(), "DELIVERY".to_string()],
                namespace: Some("awscc.logs.log_group".to_string()),
                to_dsl: None,
            })
                .with_description("Specifies the log group class for this log group. There are two classes: + The ``Standard`` log class supports all CWL features. + The ``Infrequent Ac...")
                .with_provider_name("LogGroupClass")
                .with_default(Value::String("STANDARD".to_string())),
        )
        .attribute(
            AttributeSchema::new("log_group_name", AttributeType::Custom {
                name: "String(pattern, len: 1..=512)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_pattern_b6dfbc56753dfe38_len_1_512,
                namespace: None,
                to_dsl: None,
            })
                .create_only()
                .with_description("The name of the log group. If you don't specify a name, CFNlong generates a unique ID for the log group.")
                .with_provider_name("LogGroupName"),
        )
        .attribute(
            AttributeSchema::new("resource_policy_document", super::iam_policy_document())
                .with_description("Creates or updates a resource policy for the specified log group that allows other services to put log events to this account. A LogGroup can have 1 r...")
                .with_provider_name("ResourcePolicyDocument"),
        )
        .attribute(
            AttributeSchema::new("retention_in_days", AttributeType::Custom {
                name: "IntEnum([1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653])".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_retention_in_days_int_enum,
                namespace: None,
                to_dsl: None,
            })
                .with_description("The number of days to retain the log events in the specified log group. Possible values are: 1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545,...")
                .with_provider_name("RetentionInDays"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("An array of key-value pairs to apply to the log group. For more information, see [Tag](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/...")
                .with_provider_name("Tags"),
        )
        .with_name_attribute("log_group_name")
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
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
