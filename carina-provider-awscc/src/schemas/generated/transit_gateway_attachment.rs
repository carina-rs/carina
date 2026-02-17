//! transit_gateway_attachment schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::TransitGatewayAttachment
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

/// Returns the schema config for ec2_transit_gateway_attachment (AWS::EC2::TransitGatewayAttachment)
pub fn ec2_transit_gateway_attachment_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::TransitGatewayAttachment",
        resource_type_name: "ec2_transit_gateway_attachment",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_transit_gateway_attachment")
        .with_description("Resource Type definition for AWS::EC2::TransitGatewayAttachment")
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description("(read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("options", AttributeType::Struct {
                    name: "Options".to_string(),
                    fields: vec![
                    StructField::new("appliance_mode_support", AttributeType::Enum(vec!["enable".to_string(), "disable".to_string()])).with_description("Indicates whether to enable Ipv6 Support for Vpc Attachment. Valid Values: enable | disable").with_provider_name("ApplianceModeSupport"),
                    StructField::new("dns_support", AttributeType::Enum(vec!["enable".to_string(), "disable".to_string()])).with_description("Indicates whether to enable DNS Support for Vpc Attachment. Valid Values: enable | disable").with_provider_name("DnsSupport"),
                    StructField::new("ipv6_support", AttributeType::Enum(vec!["enable".to_string(), "disable".to_string()])).with_description("Indicates whether to enable Ipv6 Support for Vpc Attachment. Valid Values: enable | disable").with_provider_name("Ipv6Support"),
                    StructField::new("security_group_referencing_support", AttributeType::Enum(vec!["enable".to_string(), "disable".to_string()])).with_description("Indicates whether to enable Security Group referencing support for Vpc Attachment. Valid Values: enable | disable").with_provider_name("SecurityGroupReferencingSupport")
                    ],
                })
                .with_description("The options for the transit gateway vpc attachment.")
                .with_provider_name("Options"),
        )
        .attribute(
            AttributeSchema::new("subnet_ids", AttributeType::List(Box::new(super::aws_resource_id())))
                .required()
                .with_provider_name("SubnetIds"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("transit_gateway_id", super::transit_gateway_id())
                .required()
                .create_only()
                .with_provider_name("TransitGatewayId"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .required()
                .create_only()
                .with_provider_name("VpcId"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    ("ec2_transit_gateway_attachment", &[])
}
