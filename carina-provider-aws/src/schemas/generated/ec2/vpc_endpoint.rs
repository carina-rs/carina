//! vpc_endpoint schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.vpc_endpoint (Smithy: com.amazonaws.ec2)
pub fn ec2_vpc_endpoint_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::VPCEndpoint",
        resource_type_name: "ec2.vpc_endpoint",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.vpc_endpoint")
            .with_description("Describes a VPC endpoint.")
            .attribute(
                AttributeSchema::new("policy_document", super::iam_policy_document())
                    .with_description(
                        "An endpoint policy, which controls access to the service from the VPC.",
                    )
                    .with_provider_name("PolicyDocument"),
            )
            .attribute(
                AttributeSchema::new("private_dns_enabled", AttributeType::Bool)
                    .with_description(
                        "Indicate whether to associate a private hosted zone with the specified VPC.",
                    )
                    .with_provider_name("PrivateDnsEnabled"),
            )
            .attribute(
                AttributeSchema::new(
                    "route_table_ids",
                    AttributeType::unordered_list(super::route_table_id()),
                )
                .with_description(
                    "The IDs of the route tables. Routing is supported only for gateway endpoints.",
                )
                .with_provider_name("RouteTableIds"),
            )
            .attribute(
                AttributeSchema::new(
                    "security_group_ids",
                    AttributeType::unordered_list(super::security_group_id()),
                )
                .with_description(
                    "The IDs of the security groups to associate with the endpoint network interfaces.",
                )
                .with_provider_name("SecurityGroupIds"),
            )
            .attribute(
                AttributeSchema::new("service_name", AttributeType::String)
                    .required()
                    .create_only()
                    .with_description("The name of the endpoint service.")
                    .with_provider_name("ServiceName"),
            )
            .attribute(
                AttributeSchema::new(
                    "subnet_ids",
                    AttributeType::unordered_list(super::subnet_id()),
                )
                .with_description(
                    "The IDs of the subnets in which to create endpoint network interfaces.",
                )
                .with_provider_name("SubnetIds"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags to associate with the endpoint.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new("vpc_endpoint_id", super::vpc_endpoint_id())
                    .with_description("The ID of the VPC endpoint. (read-only)")
                    .with_provider_name("VpcEndpointId"),
            )
            .attribute(
                AttributeSchema::new(
                    "vpc_endpoint_type",
                    AttributeType::StringEnum {
                        name: "VpcEndpointType".to_string(),
                        values: vec!["Interface".to_string(), "Gateway".to_string()],
                        namespace: Some("aws.ec2.vpc_endpoint".to_string()),
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_description("The type of endpoint. Default: Gateway")
                .with_provider_name("VpcEndpointType"),
            )
            .attribute(
                AttributeSchema::new("vpc_id", super::vpc_id())
                    .required()
                    .create_only()
                    .with_description("The ID of the VPC.")
                    .with_provider_name("VpcId"),
            ),
    }
}

const VALID_VPC_ENDPOINT_TYPE: &[&str] = &["Interface", "Gateway"];

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.vpc_endpoint",
        &[("vpc_endpoint_type", VALID_VPC_ENDPOINT_TYPE)],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
