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
        .with_description("Specifies a VPC endpoint. A VPC endpoint provides a private connection between your VPC and an endpoint service. You can use an endpoint service provided by AWS, an MKT Partner, or another AWS accounts in your organization. For more information, see the [User Guide](https://docs.aws.amazon.com/vpc/latest/privatelink/).  An endpoint of type ``Interface`` establishes connections between the subnets in your VPC and an AWS-service, your own service, or a service hosted by another AWS-account. With an interface VPC endpoint, you specify the subnets in which to create the endpoint and the security groups to associate with the endpoint network interfaces.  An endpoint of type ``gateway`` serves as a target for a route in your route table for traffic destined for S3 or DDB. You can specify an endpoint policy for the endpoint, which controls access to the service from your VPC. You can also specify the VPC route tables that use the endpoint. For more information about connectivity to S3, see [Why can't I connect to an S3 bucket using a gateway VPC endpoint?](https://docs.aws.amazon.com/premiumsupport/knowledge-center/connect-s3-vpc-endpoint)  An endpoint of type ``GatewayLoadBalancer`` provides private connectivity between your VPC and virtual appliances from a service provider.")
        .attribute(
            AttributeSchema::new("creation_timestamp", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("CreationTimestamp"),
        )
        .attribute(
            AttributeSchema::new("dns_entries", AttributeType::unordered_list(AttributeType::String))
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
            }).with_description("Indicates whether to enable private DNS only for inbound endpoints. This option is available only for services that support both gateway and interface endpoints. It routes traffic that originates from the VPC to the gateway endpoint and traffic that originates from on-premises to the interface endpoint.").with_provider_name("PrivateDnsOnlyForInboundResolverEndpoint"),
                    StructField::new("private_dns_preference", AttributeType::StringEnum {
                name: "PrivateDnsPreference".to_string(),
                values: vec!["VERIFIED_DOMAINS_ONLY".to_string(), "ALL_DOMAINS".to_string(), "VERIFIED_DOMAINS_AND_SPECIFIED_DOMAINS".to_string(), "SPECIFIED_DOMAINS_ONLY".to_string()],
                namespace: Some("awscc.ec2.vpc_endpoint".to_string()),
                to_dsl: None,
            }).with_description("The preference for which private domains have a private hosted zone created for and associated with the specified VPC. Only supported when private DNS is enabled and when the VPC endpoint type is ServiceNetwork or Resource.").with_provider_name("PrivateDnsPreference"),
                    StructField::new("private_dns_specified_domains", AttributeType::Custom {
                name: "List(1..=10)".to_string(),
                base: Box::new(AttributeType::list(AttributeType::Custom {
                name: "String(len: 1..=255)".to_string(),
                base: Box::new(AttributeType::String),
                validate: validate_string_length_1_255,
                namespace: None,
                to_dsl: None,
            })),
                validate: validate_list_items_1_10,
                namespace: None,
                to_dsl: None,
            }).with_description("Indicates which of the private domains to create private hosted zones for and associate with the specified VPC. Only supported when private DNS is enabled and the private DNS preference is ``VERIFIED_DOMAINS_AND_SPECIFIED_DOMAINS`` or ``SPECIFIED_DOMAINS_ONLY``.").with_provider_name("PrivateDnsSpecifiedDomains")
                    ],
                })
                .create_only()
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
            AttributeSchema::new("network_interface_ids", AttributeType::unordered_list(super::network_interface_id()))
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("NetworkInterfaceIds"),
        )
        .attribute(
            AttributeSchema::new("policy_document", super::iam_policy_document())
                .with_description("An endpoint policy, which controls access to the service from the VPC. The default endpoint policy allows full access to the service. Endpoint policies are supported only for gateway and interface endpoints. For CloudFormation templates in YAML, you can provide the policy in JSON or YAML format. For example, if you have a JSON policy, you can convert it to YAML before including it in the YAML template, and CFNlong converts the policy to JSON format before calling the API actions for privatelink. Alternatively, you can include the JSON directly in the YAML, as shown in the following ``Properties`` section: ``Properties: VpcEndpointType: 'Interface' ServiceName: !Sub 'com.amazonaws.${AWS::Region}.logs' PolicyDocument: '{ \"Version\":\"2012-10-17\", \"Statement\": [{ \"Effect\":\"Allow\", \"Principal\":\"*\", \"Action\":[\"logs:Describe*\",\"logs:Get*\",\"logs:List*\",\"logs:FilterLogEvents\"], \"Resource\":\"*\" }] }'``")
                .with_provider_name("PolicyDocument"),
        )
        .attribute(
            AttributeSchema::new("private_dns_enabled", AttributeType::Bool)
                .with_description("Indicate whether to associate a private hosted zone with the specified VPC. The private hosted zone contains a record set for the default public DNS name for the service for the Region (for example, ``kinesis.us-east-1.amazonaws.com``), which resolves to the private IP addresses of the endpoint network interfaces in the VPC. This enables you to make requests to the default public DNS name for the service instead of the public DNS names that are automatically generated by the VPC endpoint service. To use a private hosted zone, you must set the following VPC attributes to ``true``: ``enableDnsHostnames`` and ``enableDnsSupport``. This property is supported only for interface endpoints. Default: ``false``")
                .with_provider_name("PrivateDnsEnabled"),
        )
        .attribute(
            AttributeSchema::new("resource_configuration_arn", super::arn())
                .create_only()
                .with_description("The Amazon Resource Name (ARN) of the resource configuration.")
                .with_provider_name("ResourceConfigurationArn"),
        )
        .attribute(
            AttributeSchema::new("route_table_ids", AttributeType::unordered_list(super::route_table_id()))
                .with_description("The IDs of the route tables. Routing is supported only for gateway endpoints.")
                .with_provider_name("RouteTableIds"),
        )
        .attribute(
            AttributeSchema::new("security_group_ids", AttributeType::unordered_list(super::security_group_id()))
                .with_description("The IDs of the security groups to associate with the endpoint network interfaces. If this parameter is not specified, we use the default security group for the VPC. Security groups are supported only for interface endpoints.")
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
            AttributeSchema::new("subnet_ids", AttributeType::unordered_list(super::subnet_id()))
                .with_description("The IDs of the subnets in which to create endpoint network interfaces. You must specify this property for an interface endpoint or a Gateway Load Balancer endpoint. You can't specify this property for a gateway endpoint. For a Gateway Load Balancer endpoint, you can specify only one subnet.")
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
