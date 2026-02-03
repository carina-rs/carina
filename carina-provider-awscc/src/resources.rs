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

define_resource_type!(VpcType, "vpc");
define_resource_type!(SubnetType, "subnet");
define_resource_type!(InternetGatewayType, "internet_gateway");
define_resource_type!(VpcGatewayAttachmentType, "vpc_gateway_attachment");
define_resource_type!(RouteTableType, "route_table");
define_resource_type!(RouteType, "route");
define_resource_type!(RouteTableAssociationType, "route_table_association");
define_resource_type!(EipType, "eip");
define_resource_type!(NatGatewayType, "nat_gateway");
define_resource_type!(SecurityGroupType, "security_group");
define_resource_type!(SecurityGroupIngressRuleType, "security_group.ingress_rule");
define_resource_type!(VpcEndpointType, "vpc_endpoint");

/// Returns all resource types supported by this provider
pub fn resource_types() -> Vec<Box<dyn ResourceType>> {
    vec![
        Box::new(VpcType),
        Box::new(SubnetType),
        Box::new(InternetGatewayType),
        Box::new(VpcGatewayAttachmentType),
        Box::new(RouteTableType),
        Box::new(RouteType),
        Box::new(RouteTableAssociationType),
        Box::new(EipType),
        Box::new(NatGatewayType),
        Box::new(SecurityGroupType),
        Box::new(SecurityGroupIngressRuleType),
        Box::new(VpcEndpointType),
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
// VPC Resources
// =============================================================================

pub const VPC_CONFIG: ResourceConfig = ResourceConfig {
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

pub const SUBNET_CONFIG: ResourceConfig = ResourceConfig {
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

pub const INTERNET_GATEWAY_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::InternetGateway",
    attributes: &[
        ("internet_gateway_id", "InternetGatewayId", false), // Read-only identifier
    ],
    has_tags: true,
};

pub const VPC_GATEWAY_ATTACHMENT_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::VPCGatewayAttachment",
    attributes: &[
        ("vpc_id", "VpcId", true),
        ("internet_gateway_id", "InternetGatewayId", false),
        ("vpn_gateway_id", "VpnGatewayId", false),
    ],
    has_tags: false,
};

// =============================================================================
// Route Resources
// =============================================================================

pub const ROUTE_TABLE_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::RouteTable",
    attributes: &[
        ("route_table_id", "RouteTableId", false), // Read-only identifier
        ("vpc_id", "VpcId", true),
    ],
    has_tags: true,
};

pub const ROUTE_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::Route",
    attributes: &[
        ("route_table_id", "RouteTableId", true),
        ("destination_cidr_block", "DestinationCidrBlock", true),
        ("gateway_id", "GatewayId", false),
        ("nat_gateway_id", "NatGatewayId", false),
    ],
    has_tags: false,
};

pub const ROUTE_TABLE_ASSOCIATION_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::SubnetRouteTableAssociation",
    attributes: &[
        ("id", "Id", false), // Read-only identifier
        ("subnet_id", "SubnetId", true),
        ("route_table_id", "RouteTableId", true),
    ],
    has_tags: false,
};

// =============================================================================
// NAT / EIP Resources
// =============================================================================

pub const EIP_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::EIP",
    attributes: &[
        ("allocation_id", "AllocationId", false), // Read-only identifier
        ("domain", "Domain", false),
        ("public_ip", "PublicIp", false),
    ],
    has_tags: true,
};

pub const NAT_GATEWAY_CONFIG: ResourceConfig = ResourceConfig {
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
// Security Group Resources
// =============================================================================

pub const SECURITY_GROUP_CONFIG: ResourceConfig = ResourceConfig {
    aws_type_name: "AWS::EC2::SecurityGroup",
    attributes: &[
        ("group_id", "GroupId", false), // Read-only identifier (security group ID)
        ("vpc_id", "VpcId", true),
        ("description", "GroupDescription", false),
        ("group_name", "GroupName", false),
    ],
    has_tags: true,
};

pub const SECURITY_GROUP_INGRESS_CONFIG: ResourceConfig = ResourceConfig {
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
// VPC Endpoint Resources
// =============================================================================

pub const VPC_ENDPOINT_CONFIG: ResourceConfig = ResourceConfig {
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
        "vpc" => Some(&VPC_CONFIG),
        "subnet" => Some(&SUBNET_CONFIG),
        "internet_gateway" => Some(&INTERNET_GATEWAY_CONFIG),
        "vpc_gateway_attachment" => Some(&VPC_GATEWAY_ATTACHMENT_CONFIG),
        "route_table" => Some(&ROUTE_TABLE_CONFIG),
        "route" => Some(&ROUTE_CONFIG),
        "route_table_association" => Some(&ROUTE_TABLE_ASSOCIATION_CONFIG),
        "eip" => Some(&EIP_CONFIG),
        "nat_gateway" => Some(&NAT_GATEWAY_CONFIG),
        "security_group" => Some(&SECURITY_GROUP_CONFIG),
        "security_group.ingress_rule" => Some(&SECURITY_GROUP_INGRESS_CONFIG),
        "vpc_endpoint" => Some(&VPC_ENDPOINT_CONFIG),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_resource_config() {
        assert!(get_resource_config("vpc").is_some());
        assert!(get_resource_config("subnet").is_some());
        assert!(get_resource_config("unknown").is_none());
    }

    #[test]
    fn test_resource_config_aws_type() {
        assert_eq!(
            get_resource_config("vpc").unwrap().aws_type_name,
            "AWS::EC2::VPC"
        );
        assert_eq!(
            get_resource_config("subnet").unwrap().aws_type_name,
            "AWS::EC2::Subnet"
        );
        assert_eq!(
            get_resource_config("security_group.ingress_rule")
                .unwrap()
                .aws_type_name,
            "AWS::EC2::SecurityGroupIngress"
        );
    }
}
