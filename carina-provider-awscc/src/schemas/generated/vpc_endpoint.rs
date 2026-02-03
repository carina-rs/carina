//! vpc_endpoint schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPCEndpoint
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use super::validate_namespaced_enum;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

const VALID_IP_ADDRESS_TYPE: &[&str] = &["ipv4", "ipv6", "dualstack", "not-specified"];

fn validate_ip_address_type(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "IpAddressType",
        "awscc.ec2_vpc_endpoint",
        VALID_IP_ADDRESS_TYPE,
    )
}

const VALID_VPC_ENDPOINT_TYPE: &[&str] = &[
    "Interface",
    "Gateway",
    "GatewayLoadBalancer",
    "ServiceNetwork",
    "Resource",
];

fn validate_vpc_endpoint_type(value: &Value) -> Result<(), String> {
    validate_namespaced_enum(
        value,
        "VpcEndpointType",
        "awscc.ec2_vpc_endpoint",
        VALID_VPC_ENDPOINT_TYPE,
    )
}

/// Returns the schema for ec2_vpc_endpoint (AWS::EC2::VPCEndpoint)
pub fn ec2_vpc_endpoint_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_vpc_endpoint")
        .with_description("Specifies a VPC endpoint. A VPC endpoint provides a private connection between your VPC and an endpoint service. You can use an endpoint service provided by AWS, an MKT Partner, or another AWS account...")
        .attribute(
            AttributeSchema::new("route_table_ids", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("The IDs of the route tables. Routing is supported only for gateway endpoints."),
        )
        .attribute(
            AttributeSchema::new("subnet_ids", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("The IDs of the subnets in which to create endpoint network interfaces. You must specify this property for an interface endpoint or a Gateway Load Bala..."),
        )
        .attribute(
            AttributeSchema::new("resource_configuration_arn", AttributeType::String)
                .with_description("The Amazon Resource Name (ARN) of the resource configuration."),
        )
        .attribute(
            AttributeSchema::new("dns_entries", AttributeType::List(Box::new(AttributeType::String)))
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .with_description("The ID of the VPC."),
        )
        .attribute(
            AttributeSchema::new("service_name", AttributeType::String)
                .with_description("The name of the endpoint service."),
        )
        .attribute(
            AttributeSchema::new("policy_document", AttributeType::String)
                .with_description("An endpoint policy, which controls access to the service from the VPC. The default endpoint policy allows full access to the service. Endpoint policie..."),
        )
        .attribute(
            AttributeSchema::new("private_dns_enabled", AttributeType::Bool)
                .with_description("Indicate whether to associate a private hosted zone with the specified VPC. The private hosted zone contains a record set for the default public DNS n..."),
        )
        .attribute(
            AttributeSchema::new("service_region", AttributeType::String)
                .with_description("Describes a Region."),
        )
        .attribute(
            AttributeSchema::new("dns_options", AttributeType::String)
                .with_description("Describes the DNS options for an endpoint."),
        )
        .attribute(
            AttributeSchema::new("creation_timestamp", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("vpc_endpoint_type", AttributeType::Custom {
                name: "VpcEndpointType".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_vpc_endpoint_type,
                namespace: Some("awscc.ec2_vpc_endpoint".to_string()),
            })
                .with_description("The type of endpoint. Default: Gateway"),
        )
        .attribute(
            AttributeSchema::new("id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("network_interface_ids", AttributeType::List(Box::new(AttributeType::String)))
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("ip_address_type", AttributeType::Custom {
                name: "IpAddressType".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_ip_address_type,
                namespace: Some("awscc.ec2_vpc_endpoint".to_string()),
            })
                .with_description("The supported IP address types."),
        )
        .attribute(
            AttributeSchema::new("service_network_arn", AttributeType::String)
                .with_description("The Amazon Resource Name (ARN) of the service network."),
        )
        .attribute(
            AttributeSchema::new("security_group_ids", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("The IDs of the security groups to associate with the endpoint network interfaces. If this parameter is not specified, we use the default security grou..."),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags to associate with the endpoint."),
        )
}
