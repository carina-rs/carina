//! Resource type definitions for AWS Cloud Control API
//!
//! Resource types are automatically derived from generated schema configs.

use carina_core::provider::{ResourceSchema, ResourceType};

use crate::schemas::generated::configs;

/// A resource type backed by an AwsccSchemaConfig
struct AwsccResourceType {
    name: &'static str,
}

impl ResourceType for AwsccResourceType {
    fn name(&self) -> &'static str {
        self.name
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Returns all resource types supported by this provider.
/// Automatically derived from generated schema configs.
pub fn resource_types() -> Vec<Box<dyn ResourceType>> {
    configs()
        .into_iter()
        .map(|c| {
            Box::new(AwsccResourceType {
                name: c.resource_type_name,
            }) as Box<dyn ResourceType>
        })
        .collect()
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
