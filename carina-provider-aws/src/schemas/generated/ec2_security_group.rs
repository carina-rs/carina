//! security_group schema definition for AWS Cloud Control
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.security_group (Smithy: com.amazonaws.ec2)
pub fn ec2_security_group_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::SecurityGroup",
        resource_type_name: "ec2.security_group",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.security_group")
        .with_description("Describes a security group.")
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .with_description("Resource name"),
        )
        .attribute(
            AttributeSchema::new("region", super::aws_region())
                .with_description("The AWS region (inherited from provider if not specified)"),
        )
        .attribute(
            AttributeSchema::new("description", AttributeType::String)
                .required()
                .create_only()
                .with_description("A description for the security group. Constraints: Up to 255 characters in length Valid characters: a-z, A-Z, 0-9, spaces, and ._-:/()#,@[]+=&;{}!$*")
                .with_provider_name("Description"),
        )
        .attribute(
            AttributeSchema::new("group_name", AttributeType::String)
                .required()
                .create_only()
                .with_description("The name of the security group. Names are case-insensitive and must be unique within the VPC. Constraints: Up to 255 characters in length. Can't start...")
                .with_provider_name("GroupName"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .create_only()
                .with_description("The ID of the VPC. Required for a nondefault VPC.")
                .with_provider_name("VpcId"),
        )
        .attribute(
            AttributeSchema::new("group_id", super::security_group_id())
                .with_description("The ID of the security group. (read-only)")
                .with_provider_name("GroupId"),
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
    ("ec2.security_group", &[])
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
