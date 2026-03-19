//! nat_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::NatGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField, types};

const VALID_AVAILABILITY_MODE: &[&str] = &["zonal", "regional"];

const VALID_CONNECTIVITY_TYPE: &[&str] = &["public", "private"];

fn validate_secondary_private_ip_address_count_range(value: &Value) -> Result<(), String> {
    if let Value::Int(n) = value {
        if *n < 1 {
            Err(format!("Value {} is out of range 1..", n))
        } else {
            Ok(())
        }
    } else {
        Err("Expected integer".to_string())
    }
}

/// Returns the schema config for ec2_nat_gateway (AWS::EC2::NatGateway)
pub fn ec2_nat_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::NatGateway",
        resource_type_name: "ec2.nat_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.nat_gateway")
        .with_description("Specifies a network address translation (NAT) gateway in the specified subnet. You can create either a public NAT gateway or a private NAT gateway. The default is a public NAT gateway. If you create a...")
        .attribute(
            AttributeSchema::new("allocation_id", super::allocation_id())
                .create_only()
                .with_description("[Public NAT gateway only] The allocation ID of the Elastic IP address that's associated with the NAT gateway. This property is required for a public N...")
                .with_provider_name("AllocationId"),
        )
        .attribute(
            AttributeSchema::new("auto_provision_zones", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("AutoProvisionZones"),
        )
        .attribute(
            AttributeSchema::new("auto_scaling_ips", AttributeType::String)
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("AutoScalingIps"),
        )
        .attribute(
            AttributeSchema::new("availability_mode", AttributeType::StringEnum {
                name: "AvailabilityMode".to_string(),
                values: vec!["zonal".to_string(), "regional".to_string()],
                namespace: Some("awscc.ec2.nat_gateway".to_string()),
                to_dsl: None,
            })
                .create_only()
                .with_description("Indicates whether this is a zonal (single-AZ) or regional (multi-AZ) NAT gateway. A zonal NAT gateway is a NAT Gateway that provides redundancy and sc...")
                .with_provider_name("AvailabilityMode"),
        )
        .attribute(
            AttributeSchema::new("availability_zone_addresses", AttributeType::unordered_list(AttributeType::Struct {
                    name: "AvailabilityZoneAddress".to_string(),
                    fields: vec![
                    StructField::new("allocation_ids", AttributeType::unordered_list(super::allocation_id())).required().with_description("The allocation IDs of the Elastic IP addresses (EIPs) to be used for handling outbound NAT traffic in this specific Availability Zone.").with_provider_name("AllocationIds"),
                    StructField::new("availability_zone", super::availability_zone()).with_description("For regional NAT gateways only: The Availability Zone where this specific NAT gateway configuration will be active. Each AZ in a regional NAT gateway ...").with_provider_name("AvailabilityZone"),
                    StructField::new("availability_zone_id", super::availability_zone_id()).with_description("For regional NAT gateways only: The ID of the Availability Zone where this specific NAT gateway configuration will be active. Each AZ in a regional NA...").with_provider_name("AvailabilityZoneId")
                    ],
                }))
                .with_description("For regional NAT gateways only: Specifies which Availability Zones you want the NAT gateway to support and the Elastic IP addresses (EIPs) to use in e...")
                .with_provider_name("AvailabilityZoneAddresses")
                .with_block_name("availability_zone_address"),
        )
        .attribute(
            AttributeSchema::new("connectivity_type", AttributeType::StringEnum {
                name: "ConnectivityType".to_string(),
                values: vec!["public".to_string(), "private".to_string()],
                namespace: Some("awscc.ec2.nat_gateway".to_string()),
                to_dsl: None,
            })
                .create_only()
                .with_description("Indicates whether the NAT gateway supports public or private connectivity. The default is public connectivity.")
                .with_provider_name("ConnectivityType"),
        )
        .attribute(
            AttributeSchema::new("eni_id", super::network_interface_id())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("EniId"),
        )
        .attribute(
            AttributeSchema::new("max_drain_duration_seconds", AttributeType::Int)
                .with_description("The maximum amount of time to wait (in seconds) before forcibly releasing the IP addresses if connections are still in progress. Default value is 350 ...")
                .with_provider_name("MaxDrainDurationSeconds"),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", super::nat_gateway_id())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("NatGatewayId"),
        )
        .attribute(
            AttributeSchema::new("private_ip_address", types::ipv4_address())
                .create_only()
                .with_description("The private IPv4 address to assign to the NAT gateway. If you don't provide an address, a private IPv4 address will be automatically assigned.")
                .with_provider_name("PrivateIpAddress"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", super::route_table_id())
                .read_only()
                .with_description(" (read-only)")
                .with_provider_name("RouteTableId"),
        )
        .attribute(
            AttributeSchema::new("secondary_allocation_ids", AttributeType::list(super::allocation_id()))
                .with_description("Secondary EIP allocation IDs. For more information, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-wi...")
                .with_provider_name("SecondaryAllocationIds"),
        )
        .attribute(
            AttributeSchema::new("secondary_private_ip_address_count", AttributeType::Custom {
                name: "Int(1..)".to_string(),
                base: Box::new(AttributeType::Int),
                validate: validate_secondary_private_ip_address_count_range,
                namespace: None,
                to_dsl: None,
            })
                .with_description("[Private NAT gateway only] The number of secondary private IPv4 addresses you want to assign to the NAT gateway. For more information about secondary ...")
                .with_provider_name("SecondaryPrivateIpAddressCount"),
        )
        .attribute(
            AttributeSchema::new("secondary_private_ip_addresses", AttributeType::list(types::ipv4_address()))
                .with_description("Secondary private IPv4 addresses. For more information about secondary addresses, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/us...")
                .with_provider_name("SecondaryPrivateIpAddresses"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", super::subnet_id())
                .create_only()
                .with_description("The ID of the subnet in which the NAT gateway is located.")
                .with_provider_name("SubnetId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the NAT gateway.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", super::vpc_id())
                .create_only()
                .with_description("The ID of the VPC in which the NAT gateway is located.")
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
        "ec2.nat_gateway",
        &[
            ("availability_mode", VALID_AVAILABILITY_MODE),
            ("connectivity_type", VALID_CONNECTIVITY_TYPE),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
