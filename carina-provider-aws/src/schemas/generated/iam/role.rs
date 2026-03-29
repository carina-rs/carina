//! role schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.iam
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

fn validate_max_session_duration_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 3600 || *n > 43200 {
            Err(format!("Value {} is out of range 3600..=43200", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for iam.role (Smithy: com.amazonaws.iam)
pub fn iam_role_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::IAM::Role",
        resource_type_name: "iam.role",
        has_tags: true,
        schema: ResourceSchema::new("aws.iam.role")
            .with_description("An IAM role for delegating permissions to AWS services or other accounts.")
            .attribute(
                AttributeSchema::new("arn", super::iam_role_arn())
                    .read_only()
                    .with_description("The ARN of the role. (read-only)")
                    .with_provider_name("Arn"),
            )
            .attribute(
                AttributeSchema::new("assume_role_policy_document", super::iam_policy_document())
                    .required()
                    .with_description("The trust policy that defines which entities can assume the role.")
                    .with_provider_name("AssumeRolePolicyDocument"),
            )
            .attribute(
                AttributeSchema::new("description", AttributeType::String)
                    .with_description("A description of the role.")
                    .with_provider_name("Description"),
            )
            .attribute(
                AttributeSchema::new("max_session_duration", AttributeType::Custom {
                    name: "Int(3600..=43200)".to_string(),
                    base: Box::new(AttributeType::Int),
                    validate: validate_max_session_duration_range,
                    namespace: None,
                    to_dsl: None,
                })
                    .with_description("The maximum session duration (in seconds) for the role. Valid values: 3600-43200.")
                    .with_provider_name("MaxSessionDuration"),
            )
            .attribute(
                AttributeSchema::new("path", AttributeType::String)
                    .create_only()
                    .with_description("The path to the role. Defaults to /.")
                    .with_provider_name("Path")
                    .with_default(Value::String("/".to_string())),
            )
            .attribute(
                AttributeSchema::new("role_id", super::iam_role_id())
                    .read_only()
                    .with_description("The stable and unique string identifying the role. (read-only)")
                    .with_provider_name("RoleId"),
            )
            .attribute(
                AttributeSchema::new("role_name", AttributeType::String)
                    .create_only()
                    .with_description("A name for the IAM role, up to 64 characters in length.")
                    .with_provider_name("RoleName"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags for the role.")
                    .with_provider_name("Tags"),
            )
            .with_name_attribute("role_name"),
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("iam.role", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
