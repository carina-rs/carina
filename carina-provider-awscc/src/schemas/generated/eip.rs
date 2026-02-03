//! ec2_eip schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::EIP
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema for ec2_eip (AWS::EC2::EIP)
pub fn ec2_eip_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.ec2_eip")
        .with_description("Specifies an Elastic IP (EIP) address and can, optionally, associate it with an Amazon EC2 instance.  You can allocate an Elastic IP address from an address pool owned by AWS or from an address pool c...")
        .attribute(
            AttributeSchema::new("public_ip", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("network_border_group", AttributeType::String)
                .with_description("A unique set of Availability Zones, Local Zones, or Wavelength Zones from which AWS advertises IP addresses. Use this parameter to limit the IP addres..."),
        )
        .attribute(
            AttributeSchema::new("public_ipv4_pool", AttributeType::String)
                .with_description("The ID of an address pool that you own. Use this parameter to let Amazon EC2 select an address from the address pool.  Updates to the ``PublicIpv4Pool..."),
        )
        .attribute(
            AttributeSchema::new("allocation_id", AttributeType::String)
                .with_description(" (read-only)"),
        )
        .attribute(
            AttributeSchema::new("domain", AttributeType::String)
                .with_description("The network (``vpc``). If you define an Elastic IP address and associate it with a VPC that is defined in the same template, you must declare a depend..."),
        )
        .attribute(
            AttributeSchema::new("ipam_pool_id", AttributeType::String)
                .with_description(""),
        )
        .attribute(
            AttributeSchema::new("instance_id", AttributeType::String)
                .with_description("The ID of the instance.  Updates to the ``InstanceId`` property may require *some interruptions*. Updates on an EIP reassociates the address on its as..."),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("Any tags assigned to the Elastic IP address.  Updates to the ``Tags`` property may require *some interruptions*. Updates on an EIP reassociates the ad..."),
        )
        .attribute(
            AttributeSchema::new("transfer_address", AttributeType::String)
                .with_description("The Elastic IP address you are accepting for transfer. You can only accept one transferred address. For more information on Elastic IP address transfe..."),
        )
        .attribute(
            AttributeSchema::new("address", AttributeType::String)
                .with_description(""),
        )
}
