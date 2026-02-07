//! Resource type definitions for AWS Cloud Control API
//!
//! This module defines resource type definitions implementing the ResourceType trait.

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

#[cfg(test)]
mod tests {
    use crate::schemas::generated::{AwsccSchemaConfig, configs};

    /// Helper to find a config by resource type
    fn get_config(resource_type: &str) -> Option<AwsccSchemaConfig> {
        configs().into_iter().find(|c| {
            c.schema
                .resource_type
                .strip_prefix("awscc.")
                .map(|t| t == resource_type)
                .unwrap_or(false)
        })
    }

    #[test]
    fn test_get_schema_config() {
        assert!(get_config("ec2_vpc").is_some());
        assert!(get_config("ec2_subnet").is_some());
        assert!(get_config("unknown").is_none());
    }

    #[test]
    fn test_schema_config_aws_type() {
        assert_eq!(
            get_config("ec2_vpc").unwrap().aws_type_name,
            "AWS::EC2::VPC"
        );
        assert_eq!(
            get_config("ec2_subnet").unwrap().aws_type_name,
            "AWS::EC2::Subnet"
        );
        assert_eq!(
            get_config("ec2_security_group_ingress")
                .unwrap()
                .aws_type_name,
            "AWS::EC2::SecurityGroupIngress"
        );
    }

    #[test]
    fn test_schema_config_has_tags() {
        assert!(get_config("ec2_vpc").unwrap().has_tags);
        assert!(get_config("ec2_subnet").unwrap().has_tags);
        assert!(!get_config("ec2_route").unwrap().has_tags);
        assert!(!get_config("ec2_vpc_gateway_attachment").unwrap().has_tags);
    }

    #[test]
    fn test_schema_config_provider_name() {
        let vpc_config = get_config("ec2_vpc").unwrap();
        let cidr_attr = vpc_config.schema.attributes.get("cidr_block").unwrap();
        assert_eq!(cidr_attr.provider_name.as_deref(), Some("CidrBlock"));
        let vpc_id_attr = vpc_config.schema.attributes.get("vpc_id").unwrap();
        assert_eq!(vpc_id_attr.provider_name.as_deref(), Some("VpcId"));
    }
}
