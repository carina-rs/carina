//! nat_gateway schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::NatGateway
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

/// Returns the schema config for ec2_nat_gateway (AWS::EC2::NatGateway)
pub fn ec2_nat_gateway_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::NatGateway",
        resource_type_name: "ec2_nat_gateway",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2_nat_gateway")
        .with_description("Specifies a network address translation (NAT) gateway in the specified subnet. You can create either a public NAT gateway or a private NAT gateway. The default is a public NAT gateway. If you create a...")
        .attribute(
            AttributeSchema::new("allocation_id", AttributeType::String)
                .with_description("[Public NAT gateway only] The allocation ID of the Elastic IP address that's associated with the NAT gateway. This property is required for a public N...")
                .with_provider_name("AllocationId"),
        )
        .attribute(
            AttributeSchema::new("auto_provision_zones", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("AutoProvisionZones"),
        )
        .attribute(
            AttributeSchema::new("auto_scaling_ips", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("AutoScalingIps"),
        )
        .attribute(
            AttributeSchema::new("availability_mode", AttributeType::String)
                .with_description("Indicates whether this is a zonal (single-AZ) or regional (multi-AZ) NAT gateway. A zonal NAT gateway is a NAT Gateway that provides redundancy and sc...")
                .with_provider_name("AvailabilityMode"),
        )
        .attribute(
            AttributeSchema::new("availability_zone_addresses", AttributeType::List(Box::new(AttributeType::Struct {
                    name: "AvailabilityZoneAddress".to_string(),
                    fields: vec![
                    StructField::new("allocation_ids", AttributeType::List(Box::new(AttributeType::String))).required().with_description("The allocation IDs of the Elastic IP addresses (EIPs) to be used for handling outbound NAT traffic in this specific Availability Zone.").with_provider_name("AllocationIds"),
                    StructField::new("availability_zone", AttributeType::String).with_description("For regional NAT gateways only: The Availability Zone where this specific NAT gateway configuration will be active. Each AZ in a regional NAT gateway ...").with_provider_name("AvailabilityZone"),
                    StructField::new("availability_zone_id", AttributeType::String).with_description("For regional NAT gateways only: The ID of the Availability Zone where this specific NAT gateway configuration will be active. Each AZ in a regional NA...").with_provider_name("AvailabilityZoneId")
                    ],
                })))
                .with_description("For regional NAT gateways only: Specifies which Availability Zones you want the NAT gateway to support and the Elastic IP addresses (EIPs) to use in e...")
                .with_provider_name("AvailabilityZoneAddresses"),
        )
        .attribute(
            AttributeSchema::new("connectivity_type", AttributeType::String)
                .with_description("Indicates whether the NAT gateway supports public or private connectivity. The default is public connectivity.")
                .with_provider_name("ConnectivityType"),
        )
        .attribute(
            AttributeSchema::new("eni_id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("EniId"),
        )
        .attribute(
            AttributeSchema::new("max_drain_duration_seconds", AttributeType::Int)
                .with_description("The maximum amount of time to wait (in seconds) before forcibly releasing the IP addresses if connections are still in progress. Default value is 350 ...")
                .with_provider_name("MaxDrainDurationSeconds"),
        )
        .attribute(
            AttributeSchema::new("nat_gateway_id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("NatGatewayId"),
        )
        .attribute(
            AttributeSchema::new("private_ip_address", AttributeType::String)
                .with_description("The private IPv4 address to assign to the NAT gateway. If you don't provide an address, a private IPv4 address will be automatically assigned.")
                .with_provider_name("PrivateIpAddress"),
        )
        .attribute(
            AttributeSchema::new("route_table_id", AttributeType::String)
                .with_description(" (read-only)")
                .with_provider_name("RouteTableId"),
        )
        .attribute(
            AttributeSchema::new("secondary_allocation_ids", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("Secondary EIP allocation IDs. For more information, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-wi...")
                .with_provider_name("SecondaryAllocationIds"),
        )
        .attribute(
            AttributeSchema::new("secondary_private_ip_address_count", AttributeType::Int)
                .with_description("[Private NAT gateway only] The number of secondary private IPv4 addresses you want to assign to the NAT gateway. For more information about secondary ...")
                .with_provider_name("SecondaryPrivateIpAddressCount"),
        )
        .attribute(
            AttributeSchema::new("secondary_private_ip_addresses", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("Secondary private IPv4 addresses. For more information about secondary addresses, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/us...")
                .with_provider_name("SecondaryPrivateIpAddresses"),
        )
        .attribute(
            AttributeSchema::new("subnet_id", AttributeType::String)
                .with_description("The ID of the subnet in which the NAT gateway is located.")
                .with_provider_name("SubnetId"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the NAT gateway.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("The ID of the VPC in which the NAT gateway is located.")
                .with_provider_name("VpcId"),
        )
    }
}
