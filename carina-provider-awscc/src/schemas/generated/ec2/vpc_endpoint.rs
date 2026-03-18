//! vpc_endpoint schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::VPCEndpoint
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

const VALID_DNS_OPTIONS_SPECIFICATION_DNS_RECORD_IP_TYPE: &[&str] = &[
    "ipv4",
    "ipv6",
    "dualstack",
    "service-defined",
    "not-specified",
];

const VALID_DNS_OPTIONS_SPECIFICATION_PRIVATE_DNS_ONLY_FOR_INBOUND_RESOLVER_ENDPOINT: &[&str] =
    &["OnlyInboundResolver", "AllResolvers", "NotSpecified"];

const VALID_DNS_OPTIONS_SPECIFICATION_PRIVATE_DNS_PREFERENCE: &[&str] = &[
    "VERIFIED_DOMAINS_ONLY",
    "ALL_DOMAINS",
    "VERIFIED_DOMAINS_AND_SPECIFIED_DOMAINS",
    "SPECIFIED_DOMAINS_ONLY",
];

const VALID_IP_ADDRESS_TYPE: &[&str] = &["ipv4", "ipv6", "dualstack", "not-specified"];

const VALID_VPC_ENDPOINT_TYPE: &[&str] = &[
    "Interface",
    "Gateway",
    "GatewayLoadBalancer",
    "ServiceNetwork",
    "Resource",
];

fn validate_list_items_1_10(value: &Value) -> Result<(), String> {
    if let Value::List(items) = value {
        let len = items.len();
        if !(1..=10).contains(&len) {
            Err(format!("List has {} items, expected 1..=10", len))
        } else {
            Ok(())
        }
    } else {
        Err("Expected list".to_string())
    }
}

fn validate_string_length_1_255(value: &Value) -> Result<(), String> {
    if let Value::String(s) = value {
        let len = s.chars().count();
        if !(1..=255).contains(&len) {
            Err(format!("String length {} is out of range 1..=255", len))
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}

/// Returns the schema config for ec2_vpc_endpoint (AWS::EC2::VPCEndpoint)
pub fn ec2_vpc_endpoint_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::VPCEndpoint",
        resource_type_name: "ec2.vpc_endpoint",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.vpc_endpoint")
        .with_description("Specifies a VPC endpoint. A VPC endpoint provides a private connection between your VPC and an endpoint service. You can use an endpoint service provided by AWS, an MKT Partner, or another AWS account...")
        .attribute(
            AttributeSchema::new("creation_timestamp", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("CreationTimestamp"),
        )
        .attribute(
            AttributeSchema::new("dns_entries", AttributeType::List(Box::new(AttributeType::String)))
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("DnsEntries"),
        )
        .attribute(
            AttributeSchema::new("dns_options", AttributeType::Struct {
                    name: "DnsOptionsSpecification".to_string(),
                    fields: vec![
                    StructField::new("dns_record_ip_type", AttributeType::StringEnum {
                name: "DnsRecordIpType".to_string(),
                values: vec!["ipv4".to_string(), "ipv6".to_string(), "dualstack".to_string(), "service-defined".to_string(), "not-specified".to_string()],
                namespace: Some("awscc.ec2.vpc_endpoint".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            }).with_description("The DNS records created for the endpoint.").with_provider_name("DnsRecordIpType"),
                    StructField::new("private_dns_only_for_inbound_resolver_endpoint", AttributeType::StringEnum {
                name: "PrivateDnsOnlyForInboundResolverEndpoint".to_string(),
                values: vec!["OnlyInboundResolver".to_string(), "AllResolvers".to_string(), "NotSpecified".to_string()],
                namespace: Some("awscc.ec2.vpc_endpoint".to_string()),
                to_dsl: None,
            }).with_description("Indicates whether to enable private DNS only for inbound endpoints. This option is available only for services that support both gateway and interface...").with_provider_name("PrivateDnsOnlyForInboundResolverEndpoint"),
                    StructField::new("private_dns_preference", AttributeType::StringEnum {
                name: "PrivateDnsPreference".to_string(),
                values: vec!["VERIFIED_DOMAINS_ONLY".to_string(), "ALL_DOMAINS".to_string(), "VERIFIED_DOMAINS_AND_SPECIFIED_DOMAINS".to_string(), "SPECIFIED_DOMAINS_ONLY".to_string()],
                namespace: Some("awscc.ec2.vpc_endpoint".to_string()),
                to_dsl: None,
            }).with_description("The preference for which private domains have a private hosted zone created for and associated with the specified VPC. Only supported when private DNS...").with_provider_name("PrivateDnsPreference"),
                    StructField::new("private_dns_specified_domains", AttributeType::Custom {
                name: "List(1..=10)".to_string(),
                base: Box::new(AttributeType::List(Box::new(AttributeType::Custom {
                name: "String(len: 1..=255)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_1_255,
                namespace: None,
                to_dsl: None,
            }))),
                validate: validate_list_items_1_10,
                namespace: None,
                to_dsl: None,
            }).with_description("Indicates which of the private domains to create private hosted zones for and associate with the specified VPC. Only supported when private DNS is ena...").with_provider_name("PrivateDnsSpecifiedDomains")
                    ],
                })
                .with_description("Describes the DNS options for an endpoint.")
                .with_provider_name("DnsOptions"),
        )
        .attribute(
            AttributeSchema::new("id", super::vpc_endpoint_id())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("ip_address_type", AttributeType::StringEnum {
                name: "IpAddressType".to_string(),
                values: vec!["ipv4".to_string(), "ipv6".to_string(), "dualstack".to_string(), "not-specified".to_string()],
                namespace: Some("awscc.ec2.vpc_endpoint".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })
                .with_description("The supported IP address types.")
                .with_provider_name("IpAddressType"),
        )
        .attribute(
            AttributeSchema::new("network_interface_ids", AttributeType::List(Box::new(super::network_interface_id())))
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("NetworkInterfaceIds"),
        )
        .attribute(
            AttributeSchema::new("policy_document", super::iam_policy_document())
                .with_description("An endpoint policy, which controls access to the service from the VPC. The default endpoint policy allows full access to the service. Endpoint policie...")
                .with_provider_name("PolicyDocument"),
        )
        .attribute(
            AttributeSchema::new("private_dns_enabled", AttributeType::Bool)
                .with_description("Indicate whether to associate a private hosted zone with the specified VPC. The private hosted zone contains a record set for the default public DNS n...")
                .with_provider_name("PrivateDnsEnabled"),
        )
        .attribute(
            AttributeSchema::new("resource_configuration_arn", super::arn())
                .create_only()
                .with_description("The Amazon Resource Name (ARN) of the resource configuration.")
                .with_provider_name("ResourceConfigurationArn"),
        )
        .attribute(
            AttributeSchema::new("route_table_ids", AttributeType::List(Box::new(super::route_table_id())))
                .with_description("The IDs of the route tables. Routing is supported only for gateway endpoints.")
                .with_provider_name("RouteTableIds"),
        )
        .attribute(
            AttributeSchema::new("security_group_ids", AttributeType::List(Box::new(super::security_group_id())))
                .with_description("The IDs of the security groups to associate with the endpoint network interfaces. If this parameter is not specified, we use the default security grou...")
                .with_provider_name("SecurityGroupIds"),
        )
        .attribute(
            AttributeSchema::new("service_name", AttributeType::String)
                .create_only()
                .with_description("The name of the endpoint service.")
                .with_provider_name("ServiceName"),
        )
        .attribute(
            AttributeSchema::new("service_network_arn", super::arn())
                .create_only()
                .with_description("The Amazon Resource Name (ARN) of the service network.")
                .with_provider_name("ServiceNetworkArn"),
        )
        .attribute(
            AttributeSchema::new("service_region", super::awscc_region())
                .create_only()
                .with_description("Describes a Region.")
                .with_provider_name("ServiceRegion"),
        )
        .attribute(
            AttributeSchema::new("subnet_ids", AttributeType::List(Box::new(super::subnet_id())))
                .with_description("The IDs of the subnets in which to create endpoint network interfaces. You must specify this property for an interface endpoint or a Gateway Load Bala...")
                .with_provider_name("SubnetIds"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags to associate with the endpoint.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("vpc_endpoint_type", AttributeType::StringEnum {
                name: "VpcEndpointType".to_string(),
                values: vec!["Interface".to_string(), "Gateway".to_string(), "GatewayLoadBalancer".to_string(), "ServiceNetwork".to_string(), "Resource".to_string()],
                namespace: Some("awscc.ec2.vpc_endpoint".to_string()),
                to_dsl: None,
            })
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
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.vpc_endpoint",
        &[
            (
                "dns_record_ip_type",
                VALID_DNS_OPTIONS_SPECIFICATION_DNS_RECORD_IP_TYPE,
            ),
            (
                "private_dns_only_for_inbound_resolver_endpoint",
                VALID_DNS_OPTIONS_SPECIFICATION_PRIVATE_DNS_ONLY_FOR_INBOUND_RESOLVER_ENDPOINT,
            ),
            (
                "private_dns_preference",
                VALID_DNS_OPTIONS_SPECIFICATION_PRIVATE_DNS_PREFERENCE,
            ),
            ("ip_address_type", VALID_IP_ADDRESS_TYPE),
            ("vpc_endpoint_type", VALID_VPC_ENDPOINT_TYPE),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
