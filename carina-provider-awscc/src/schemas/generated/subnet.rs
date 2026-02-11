//! subnet schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::Subnet
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeError, types, validators,
};
use std::collections::HashMap;

/// Validator for EC2 Subnet: requires exactly one of cidr_block or ipv4_ipam_pool_id
fn validate_subnet(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
    validators::validate_exclusive_required(attributes, &["cidr_block", "ipv4_ipam_pool_id"])
}

/// Returns the schema config for ec2_subnet (AWS::EC2::Subnet)
pub fn ec2_subnet_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::Subnet",
        resource_type_name: "ec2_subnet",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_subnet")
        .with_description("Specifies a subnet for the specified VPC.  For an IPv4 only subnet, specify an IPv4 CIDR block. If the VPC has an IPv6 CIDR block, you can create an IPv6 only subnet or a dual stack subnet instead. Fo...")
        .attribute(
            AttributeSchema::new("assign_ipv6_address_on_creation", AttributeType::Bool)
                .with_description("Indicates whether a network interface created in this subnet receives an IPv6 address. The default value is ``false``. If you specify ``AssignIpv6Addr...")
                .with_provider_name("AssignIpv6AddressOnCreation"),
        )
        .attribute(
            AttributeSchema::new("availability_zone", super::availability_zone())
                .with_description("The Availability Zone of the subnet. If you update this property, you must also update the ``CidrBlock`` property.")
                .with_provider_name("AvailabilityZone"),
        )
        .attribute(
            AttributeSchema::new("availability_zone_id", AttributeType::String)
                .with_description("The AZ ID of the subnet.")
                .with_provider_name("AvailabilityZoneId"),
        )
        .attribute(
            AttributeSchema::new("block_public_access_states", AttributeType::Struct {
                    name: "BlockPublicAccessStates".to_string(),
                    fields: vec![
                    StructField::new("internet_gateway_block_mode", AttributeType::Enum(vec!["off".to_string(), "block-bidirectional".to_string(), "block-ingress".to_string()])).with_description("The mode of VPC BPA. Options here are off, block-bidirectional, block-ingress ").with_provider_name("InternetGatewayBlockMode")
                    ],
                })
                .with_description(" (read-only)")
                .with_provider_name("BlockPublicAccessStates"),
        )
        .attribute(
            AttributeSchema::new("cidr_block", types::ipv4_cidr())
                .with_description("The IPv4 CIDR block assigned to the subnet. If you update this property, we create a new subnet, and then delete the existing one.")
                .with_provider_name("CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("enable_dns64", AttributeType::Bool)
                .with_description("Indicates whether DNS queries made to the Amazon-provided DNS Resolver in this subnet should return synthetic IPv6 addresses for IPv4-only destination...")
                .with_provider_name("EnableDns64"),
        )
        .attribute(
            AttributeSchema::new("enable_lni_at_device_index", AttributeType::Int)
                .with_description("Indicates the device position for local network interfaces in this subnet. For example, ``1`` indicates local network interfaces in this subnet are th...")
                .with_provider_name("EnableLniAtDeviceIndex"),
        )
        .attribute(
            AttributeSchema::new("ipv4_ipam_pool_id", super::ipam_pool_id())
                .with_description("An IPv4 IPAM pool ID for the subnet.")
                .with_provider_name("Ipv4IpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("ipv4_netmask_length", AttributeType::Int)
                .with_description("An IPv4 netmask length for the subnet.")
                .with_provider_name("Ipv4NetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_block", types::ipv6_cidr())
                .with_description("The IPv6 CIDR block. If you specify ``AssignIpv6AddressOnCreation``, you must also specify an IPv6 CIDR block.")
                .with_provider_name("Ipv6CidrBlock"),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_blocks", AttributeType::List(Box::new(types::ipv6_cidr())))
                .with_description(" (read-only)")
                .with_provider_name("Ipv6CidrBlocks"),
        )
        .attribute(
            AttributeSchema::new("ipv6_ipam_pool_id", super::ipam_pool_id())
                .with_description("An IPv6 IPAM pool ID for the subnet.")
                .with_provider_name("Ipv6IpamPoolId"),
        )
        .attribute(
            AttributeSchema::new("ipv6_native", AttributeType::Bool)
                .with_description("Indicates whether this is an IPv6 only subnet. For more information, see [Subnet basics](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Subnets....")
                .with_provider_name("Ipv6Native"),
        )
        .attribute(
            AttributeSchema::new("ipv6_netmask_length", AttributeType::Int)
                .with_description("An IPv6 netmask length for the subnet.")
                .with_provider_name("Ipv6NetmaskLength"),
        )
        .attribute(
            AttributeSchema::new("map_public_ip_on_launch", AttributeType::Bool)
                .with_description("Indicates whether instances launched in this subnet receive a public IPv4 address. The default value is ``false``. AWS charges for all public IPv4 add...")
                .with_provider_name("MapPublicIpOnLaunch"),
        )
        .attribute(
            AttributeSchema::new("network_acl_association_id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("NetworkAclAssociationId"),
        )
        .attribute(
            AttributeSchema::new("outpost_arn", super::arn())
                .with_description("The Amazon Resource Name (ARN) of the Outpost.")
                .with_provider_name("OutpostArn"),
        )
        .attribute(
            AttributeSchema::new("private_dns_name_options_on_launch", AttributeType::Struct {
                    name: "PrivateDnsNameOptionsOnLaunch".to_string(),
                    fields: vec![
                    StructField::new("enable_resource_name_dns_aaaa_record", AttributeType::Bool).with_provider_name("EnableResourceNameDnsAAAARecord"),
                    StructField::new("enable_resource_name_dns_a_record", AttributeType::Bool).with_provider_name("EnableResourceNameDnsARecord"),
                    StructField::new("hostname_type", AttributeType::Enum(vec!["ip-name".to_string(), "resource-name".to_string()])).with_provider_name("HostnameType")
                    ],
                })
                .with_description("The hostname type for EC2 instances launched into this subnet and how DNS A and AAAA record queries to the instances should be handled. For more infor...")
                .with_provider_name("PrivateDnsNameOptionsOnLaunch"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", super::subnet_id())
                .with_description(" (read-only)")
                .with_provider_name("SubnetId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the subnet.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .required()
                .with_description("The ID of the VPC the subnet is in. If you update this property, you must also update the ``CidrBlock`` property.")
                .with_provider_name("VpcId"),
        )
        .with_validator(validate_subnet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subnet_requires_cidr_or_ipam() {
        let config = ec2_subnet_config();
        let schema = config.schema;

        // Valid: has cidr_block
        let mut attrs1 = HashMap::new();
        attrs1.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );
        attrs1.insert(
            "cidr_block".to_string(),
            Value::String("10.0.1.0/24".to_string()),
        );
        assert!(
            schema.validate(&attrs1).is_ok(),
            "Subnet with cidr_block should be valid"
        );

        // Valid: has ipv4_ipam_pool_id
        let mut attrs2 = HashMap::new();
        attrs2.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );
        attrs2.insert(
            "ipv4_ipam_pool_id".to_string(),
            Value::String("ipam-pool-0a1b2c3d4e5f6789a".to_string()),
        );
        assert!(
            schema.validate(&attrs2).is_ok(),
            "Subnet with ipv4_ipam_pool_id should be valid"
        );

        // Invalid: has neither cidr_block nor ipv4_ipam_pool_id
        let mut attrs3 = HashMap::new();
        attrs3.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );
        let result = schema.validate(&attrs3);
        assert!(
            result.is_err(),
            "Subnet without cidr_block or ipv4_ipam_pool_id should be invalid"
        );
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Exactly one of [cidr_block, ipv4_ipam_pool_id] must be specified")
        );

        // Invalid: has both cidr_block and ipv4_ipam_pool_id
        let mut attrs4 = HashMap::new();
        attrs4.insert(
            "vpc_id".to_string(),
            Value::String("vpc-12345678".to_string()),
        );
        attrs4.insert(
            "cidr_block".to_string(),
            Value::String("10.0.1.0/24".to_string()),
        );
        attrs4.insert(
            "ipv4_ipam_pool_id".to_string(),
            Value::String("ipam-pool-0a1b2c3d4e5f6789a".to_string()),
        );
        let result = schema.validate(&attrs4);
        assert!(
            result.is_err(),
            "Subnet with both cidr_block and ipv4_ipam_pool_id should be invalid"
        );
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Only one of [cidr_block, ipv4_ipam_pool_id] can be specified")
        );
    }
}
