//! Resource type configurations for AWS Cloud Control API
//!
//! This module defines:
//! - Resource type definitions (implementing ResourceType trait)
//! - Mapping between DSL resource types and AWS CloudFormation resource types

use carina_core::provider::{ResourceSchema, ResourceType};

// =============================================================================
// Resource Type Definitions
// =============================================================================

macro_rules! define_resource_type {
    ($name:ident, $type_name:expr) => {
        pub struct $name;
        impl ResourceType for $name {
            fn name(&self) -> &'static str {
                $type_name
            }
            fn schema(&self) -> ResourceSchema {
                ResourceSchema::default()
            }
        }
    };
}

define_resource_type!(Ec2VpcType, "ec2_vpc");
define_resource_type!(Ec2SubnetType, "ec2_subnet");
define_resource_type!(Ec2InternetGatewayType, "ec2_internet_gateway");
define_resource_type!(Ec2VpcGatewayAttachmentType, "ec2_vpc_gateway_attachment");
define_resource_type!(Ec2RouteTableType, "ec2_route_table");
define_resource_type!(Ec2RouteType, "ec2_route");
define_resource_type!(
    Ec2SubnetRouteTableAssociationType,
    "ec2_subnet_route_table_association"
);
define_resource_type!(Ec2EipType, "ec2_eip");
define_resource_type!(Ec2NatGatewayType, "ec2_nat_gateway");
define_resource_type!(Ec2SecurityGroupType, "ec2_security_group");
define_resource_type!(Ec2SecurityGroupIngressType, "ec2_security_group_ingress");
define_resource_type!(Ec2VpcEndpointType, "ec2_vpc_endpoint");

/// Returns all resource types supported by this provider
pub fn resource_types() -> Vec<Box<dyn ResourceType>> {
    vec![
        Box::new(Ec2VpcType),
        Box::new(Ec2SubnetType),
        Box::new(Ec2InternetGatewayType),
        Box::new(Ec2VpcGatewayAttachmentType),
        Box::new(Ec2RouteTableType),
        Box::new(Ec2RouteType),
        Box::new(Ec2SubnetRouteTableAssociationType),
        Box::new(Ec2EipType),
        Box::new(Ec2NatGatewayType),
        Box::new(Ec2SecurityGroupType),
        Box::new(Ec2SecurityGroupIngressType),
        Box::new(Ec2VpcEndpointType),
    ]
}

// =============================================================================
// Resource Configuration
// =============================================================================

/// Attribute mapping: (dsl_name, aws_name, is_required_for_create)
pub type AttrMapping = (&'static str, &'static str, bool);

/// Resource type configuration
pub struct ResourceConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Standard attribute mappings (DSL name -> AWS name)
    pub attributes: &'static [AttrMapping],
    /// Whether this resource type uses tags
    pub has_tags: bool,
}

// =============================================================================
// EC2 VPC Resources
// =============================================================================

pub const EC2_VPC_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::VPC",
    attributes: &[
        ("vpc_id", "VpcId", false), // Read-only identifier
        ("cidr_block", "CidrBlock", true),
        ("enable_dns_hostnames", "EnableDnsHostnames", false),
        ("enable_dns_support", "EnableDnsSupport", false),
        ("instance_tenancy", "InstanceTenancy", false),
    ],
    has_tags: true,
};

pub const EC2_SUBNET_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::Subnet",
    attributes: &[
        ("subnet_id", "SubnetId", false), // Read-only identifier
        ("vpc_id", "VpcId", true),
        ("cidr_block", "CidrBlock", true),
        ("availability_zone", "AvailabilityZone", false),
        ("map_public_ip_on_launch", "MapPublicIpOnLaunch", false),
    ],
    has_tags: true,
};

pub const EC2_INTERNET_GATEWAY_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::InternetGateway",
    attributes: &[
        ("internet_gateway_id", "InternetGatewayId", false), // Read-only identifier
    ],
    has_tags: true,
};

pub const EC2_VPC_GATEWAY_ATTACHMENT_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::VPCGatewayAttachment",
    attributes: &[
        ("vpc_id", "VpcId", true),
        ("internet_gateway_id", "InternetGatewayId", false),
        ("vpn_gateway_id", "VpnGatewayId", false),
    ],
    has_tags: false,
};

// =============================================================================
// EC2 Route Resources
// =============================================================================

pub const EC2_ROUTE_TABLE_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::RouteTable",
    attributes: &[
        ("route_table_id", "RouteTableId", false), // Read-only identifier
        ("vpc_id", "VpcId", true),
    ],
    has_tags: true,
};

pub const EC2_ROUTE_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::Route",
    attributes: &[
        ("route_table_id", "RouteTableId", true),
        ("destination_cidr_block", "DestinationCidrBlock", true),
        ("gateway_id", "GatewayId", false),
        ("nat_gateway_id", "NatGatewayId", false),
    ],
    has_tags: false,
};

pub const EC2_SUBNET_ROUTE_TABLE_ASSOCIATION_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::SubnetRouteTableAssociation",
    attributes: &[
        ("id", "Id", false), // Read-only identifier
        ("subnet_id", "SubnetId", true),
        ("route_table_id", "RouteTableId", true),
    ],
    has_tags: false,
};

// =============================================================================
// EC2 NAT / EIP Resources
// =============================================================================

pub const EC2_EIP_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::EIP",
    attributes: &[
        ("allocation_id", "AllocationId", false), // Read-only identifier
        ("domain", "Domain", false),
        ("public_ip", "PublicIp", false),
    ],
    has_tags: true,
};

pub const EC2_NAT_GATEWAY_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::NatGateway",
    attributes: &[
        ("nat_gateway_id", "NatGatewayId", false), // Read-only identifier
        ("subnet_id", "SubnetId", true),
        ("allocation_id", "AllocationId", false),
        ("connectivity_type", "ConnectivityType", false),
    ],
    has_tags: true,
};

// =============================================================================
// EC2 Security Group Resources
// =============================================================================

pub const EC2_SECURITY_GROUP_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::SecurityGroup",
    attributes: &[
        ("group_id", "GroupId", false), // Read-only identifier (security group ID)
        ("vpc_id", "VpcId", true),
        ("description", "GroupDescription", false),
        ("group_name", "GroupName", false),
    ],
    has_tags: true,
};

pub const EC2_SECURITY_GROUP_INGRESS_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::SecurityGroupIngress",
    attributes: &[
        ("security_group_id", "GroupId", true),
        ("ip_protocol", "IpProtocol", true),
        ("from_port", "FromPort", false),
        ("to_port", "ToPort", false),
        ("cidr_ip", "CidrIp", false),
    ],
    has_tags: false,
};

// =============================================================================
// EC2 VPC Endpoint Resources
// =============================================================================

pub const EC2_VPC_ENDPOINT_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::VPCEndpoint",
    attributes: &[
        ("vpc_endpoint_id", "Id", false), // Read-only identifier
        ("vpc_id", "VpcId", true),
        ("service_name", "ServiceName", true),
        ("vpc_endpoint_type", "VpcEndpointType", false),
    ],
    has_tags: false,
};

// =============================================================================
// Config Lookup
// =============================================================================

/// Get resource configuration by DSL type name
pub fn get_resource_config(resource_type: &str) -> Option<&'static ResourceConfig> {
    match resource_type {
        "ec2_vpc" => Some(&EC2_VPC_CONFIG),
        "ec2_subnet" => Some(&EC2_SUBNET_CONFIG),
        "ec2_internet_gateway" => Some(&EC2_INTERNET_GATEWAY_CONFIG),
        "ec2_vpc_gateway_attachment" => Some(&EC2_VPC_GATEWAY_ATTACHMENT_CONFIG),
        "ec2_route_table" => Some(&EC2_ROUTE_TABLE_CONFIG),
        "ec2_route" => Some(&EC2_ROUTE_CONFIG),
        "ec2_subnet_route_table_association" => Some(&EC2_SUBNET_ROUTE_TABLE_ASSOCIATION_CONFIG),
        "ec2_eip" => Some(&EC2_EIP_CONFIG),
        "ec2_nat_gateway" => Some(&EC2_NAT_GATEWAY_CONFIG),
        "ec2_security_group" => Some(&EC2_SECURITY_GROUP_CONFIG),
        "ec2_security_group_ingress" => Some(&EC2_SECURITY_GROUP_INGRESS_CONFIG),
        "ec2_vpc_endpoint" => Some(&EC2_VPC_ENDPOINT_CONFIG),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_resource_config() {
        assert!(get_resource_config("ec2_vpc").is_some());
        assert!(get_resource_config("ec2_subnet").is_some());
        assert!(get_resource_config("unknown").is_none());
    }

    #[test]
    fn test_resource_config_aws_type() {
        assert_eq!(
            get_resource_config("ec2_vpc").unwrap().aws_type_name,
            "AWS::EC2::VPC"
        );
        assert_eq!(
            get_resource_config("ec2_subnet").unwrap().aws_type_name,
            "AWS::EC2::Subnet"
        );
        assert_eq!(
            get_resource_config("ec2_security_group_ingress")
                .unwrap()
                .aws_type_name,
            "AWS::EC2::SecurityGroupIngress"
        );
    }
}
