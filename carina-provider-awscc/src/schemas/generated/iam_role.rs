//! role schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::IAM::Role
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

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

/// Returns the schema config for iam_role (AWS::IAM::Role)
pub fn iam_role_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::IAM::Role",
        resource_type_name: "iam.role",
        has_tags: true,
        schema: ResourceSchema::new("awscc.iam.role")
        .with_description("Creates a new role for your AWS-account.   For more information about roles, see [IAM roles](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles.html) in the *IAM User Guide*. For information ab...")
        .attribute(
            AttributeSchema::new("arn", super::iam_role_arn())
                .with_description(" (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("assume_role_policy_document", super::iam_policy_document())
                .required()
                .with_description("The trust policy that is associated with this role. Trust policies define which entities can assume the role. You can associate only one trust policy ...")
                .with_provider_name("AssumeRolePolicyDocument"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_description("A description of the role that you provide.")
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("managed_policy_arns", AttributeType::List(Box::new(super::iam_policy_arn())))
                .with_description("A list of Amazon Resource Names (ARNs) of the IAM managed policies that you want to attach to the role. For more information about ARNs, see [Amazon R...")
                .with_provider_name("ManagedPolicyArns"),
        )
        .attribute(
            AttributeSchema::new("max_session_duration", AttributeType::Custom {
                name: "Int(3600..=43200)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_max_session_duration_range,
                namespace: None,
                to_dsl: None,
            })
                .with_description("The maximum session duration (in seconds) that you want to set for the specified role. If you do not specify a value for this setting, the default val...")
                .with_provider_name("MaxSessionDuration"),
        )
        .attribute(
            AttributeSchema::new("path", AttributeType::String)
                .create_only()
                .with_description("The path to the role. For more information about paths, see [IAM Identifiers](https://docs.aws.amazon.com/IAM/latest/UserGuide/Using_Identifiers.html)...")
                .with_provider_name("Path"),
        )
        .attribute(
            AttributeSchema::new("permissions_boundary", super::iam_policy_arn())
                .with_description("The ARN of the policy used to set the permissions boundary for the role. For more information about permissions boundaries, see [Permissions boundarie...")
                .with_provider_name("PermissionsBoundary"),
        )
        .attribute(
            AttributeSchema::new("policies", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "Policy".to_string(),
                    fields: vec![
                    StructField::new("policy_document", super::iam_policy_document()).required().with_description("The entire contents of the policy that defines permissions. For more information, see [Overview of JSON policies](https://docs.aws.amazon.com/IAM/late...").with_provider_name("PolicyDocument"),
                    StructField::new("policy_name", AttributeType::String).required().with_description("The friendly name (not ARN) identifying the policy.").with_provider_name("PolicyName")
                    ],
                })))
                .with_description("Adds or updates an inline policy document that is embedded in the specified IAM role. When you embed an inline policy in a role, the inline policy is ...")
                .with_provider_name("Policies"),
        )
        .attribute(
            AttributeSchema::new("role_id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("RoleId"),
        )
        .attribute(
            AttributeSchema::new("role_name", AttributeType::String)
                .create_only()
                .with_description("A name for the IAM role, up to 64 characters in length. For valid values, see the ``RoleName`` parameter for the [CreateRole](https://docs.aws.amazon....")
                .with_provider_name("RoleName"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("A list of tags that are attached to the role. For more information about tagging, see [Tagging IAM resources](https://docs.aws.amazon.com/IAM/latest/U...")
                .with_provider_name("Tags"),
        )
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
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
