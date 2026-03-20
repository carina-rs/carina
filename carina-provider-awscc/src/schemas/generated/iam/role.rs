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
        .with_description("Creates a new role for your AWS-account.   For more information about roles, see [IAM roles](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles.html) in the *IAM User Guide*. For information about quotas for role names and the number of roles you can create, see [IAM and quotas](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_iam-quotas.html) in the *IAM User Guide*.")
        .attribute(
            AttributeSchema::new("arn", super::iam_role_arn())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("Arn"),
        )
        .attribute(
            AttributeSchema::new("assume_role_policy_document", super::iam_policy_document())
                .required()
                .with_description("The trust policy that is associated with this role. Trust policies define which entities can assume the role. You can associate only one trust policy with a role. For an example of a policy that can be used to assume a role, see [Template Examples](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-iam-role.html#aws-resource-iam-role--examples). For more information about the elements that you can use in an IAM policy, see [Policy Elements Reference](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_elements.html) in the *User Guide*.")
                .with_provider_name("AssumeRolePolicyDocument"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .with_description("A description of the role that you provide.")
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("managed_policy_arns", AttributeType::unordered_list(super::iam_policy_arn()))
                .with_description("A list of Amazon Resource Names (ARNs) of the IAM managed policies that you want to attach to the role. For more information about ARNs, see [Amazon Resource Names (ARNs) and Service Namespaces](https://docs.aws.amazon.com/general/latest/gr/aws-arns-and-namespaces.html) in the *General Reference*.")
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
                .with_description("The maximum session duration (in seconds) that you want to set for the specified role. If you do not specify a value for this setting, the default value of one hour is applied. This setting can have a value from 1 hour to 12 hours. Anyone who assumes the role from the CLI or API can use the ``DurationSeconds`` API parameter or the ``duration-seconds``CLI parameter to request a longer session. The ``MaxSessionDuration`` setting determines the maximum duration that can be requested using the ``DurationSeconds`` parameter. If users don't specify a value for the ``DurationSeconds`` parameter, their security credentials are valid for one hour by default. This applies when you use the ``AssumeRole*`` API operations or the ``assume-role*``CLI operations but does not apply when you use those operations to create a console URL. For more information, see [Using IAM roles](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_use.html) in the *IAM User Guide*.")
                .with_provider_name("MaxSessionDuration"),
        )
        .attribute(
            AttributeSchema::new("path", AttributeType::String)
                .create_only()
                .with_description("The path to the role. For more information about paths, see [IAM Identifiers](https://docs.aws.amazon.com/IAM/latest/UserGuide/Using_Identifiers.html) in the *IAM User Guide*. This parameter is optional. If it is not included, it defaults to a slash (/). This parameter allows (through its [regex pattern](https://docs.aws.amazon.com/http://wikipedia.org/wiki/regex)) a string of characters consisting of either a forward slash (/) by itself or a string that must begin and end with forward slashes. In addition, it can contain any ASCII character from the ! (``\\u0021``) through the DEL character (``\\u007F``), including most punctuation characters, digits, and upper and lowercased letters.")
                .with_provider_name("Path")
                .with_default(Value::String("/".to_string())),
        )
        .attribute(
            AttributeSchema::new("permissions_boundary", super::iam_policy_arn())
                .with_description("The ARN of the policy used to set the permissions boundary for the role. For more information about permissions boundaries, see [Permissions boundaries for IAM identities](https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies_boundaries.html) in the *IAM User Guide*.")
                .with_provider_name("PermissionsBoundary"),
        )
        .attribute(
            AttributeSchema::new("policies", AttributeType::unordered_list(AttributeType::Struct {
                    name: "Policy".to_string(),
                    fields: vec![
                    StructField::new("policy_document", super::iam_policy_document()).required().with_description("The entire contents of the policy that defines permissions. For more information, see [Overview of JSON policies](https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies.html#access_policies-json).").with_provider_name("PolicyDocument"),
                    StructField::new("policy_name", AttributeType::String).required().with_description("The friendly name (not ARN) identifying the policy.").with_provider_name("PolicyName")
                    ],
                }))
                .with_description("Adds or updates an inline policy document that is embedded in the specified IAM role. When you embed an inline policy in a role, the inline policy is used as part of the role's access (permissions) policy. The role's trust policy is created at the same time as the role. You can update a role's trust policy later. For more information about IAM roles, go to [Using Roles to Delegate Permissions and Federate Identities](https://docs.aws.amazon.com/IAM/latest/UserGuide/roles-toplevel.html). A role can also have an attached managed policy. For information about policies, see [Managed Policies and Inline Policies](https://docs.aws.amazon.com/IAM/latest/UserGuide/policies-managed-vs-inline.html) in the *User Guide*. For information about limits on the number of inline policies that you can embed with a role, see [Limitations on Entities](https://docs.aws.amazon.com/IAM/latest/UserGuide/LimitationsOnEntities.html) in the *User Guide*. If an external policy (such as ``AWS::IAM::Policy`` or ``AWS::IAM::ManagedPolicy``) has a ``Ref`` to a role and if a resource (such as ``AWS::ECS::Service``) also has a ``Ref`` to the same role, add a ``DependsOn`` attribute to the resource to make the resource depend on the external policy. This dependency ensures that the role's policy is available throughout the resource's lifecycle. For example, when you delete a stack with an ``AWS::ECS::Service`` resource, the ``DependsOn`` attribute ensures that CFN deletes the ``AWS::ECS::Service`` resource before deleting its role's policy.")
                .with_provider_name("Policies")
                .with_block_name("policy"),
        )
        .attribute(
            AttributeSchema::new("role_id", super::iam_role_id())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("RoleId"),
        )
        .attribute(
            AttributeSchema::new("role_name", AttributeType::String)
                .create_only()
                .with_description("A name for the IAM role, up to 64 characters in length. For valid values, see the ``RoleName`` parameter for the [CreateRole](https://docs.aws.amazon.com/IAM/latest/APIReference/API_CreateRole.html) action in the *User Guide*. This parameter allows (per its [regex pattern](https://docs.aws.amazon.com/http://wikipedia.org/wiki/regex)) a string of characters consisting of upper and lowercase alphanumeric characters with no spaces. You can also include any of the following characters: _+=,.@-. The role name must be unique within the account. Role names are not distinguished by case. For example, you cannot create roles named both \"Role1\" and \"role1\". If you don't specify a name, CFN generates a unique physical ID and uses that ID for the role name. If you specify a name, you must specify the ``CAPABILITY_NAMED_IAM`` value to acknowledge your template's capabilities. For more information, see [Acknowledging Resources in Templates](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/using-iam-template.html#using-iam-capabilities). Naming an IAM resource can cause an unrecoverable error if you reuse the same template in multiple Regions. To prevent this, we recommend using ``Fn::Join`` and ``AWS::Region`` to create a Region-specific name, as in the following example: ``{\"Fn::Join\": [\"\", [{\"Ref\": \"AWS::Region\"}, {\"Ref\": \"MyResourceName\"}]]}``.")
                .with_provider_name("RoleName"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("A list of tags that are attached to the role. For more information about tagging, see [Tagging IAM resources](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_tags.html) in the *IAM User Guide*.")
                .with_provider_name("Tags"),
        )
        .with_name_attribute("role_name")
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
