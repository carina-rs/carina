//! VPC resource schema definition for AWS Cloud Control
//!
//! Based on CloudFormation AWS::EC2::VPC schema:
//! https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpc.html

use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, CompletionValue, ResourceSchema, types};

/// Valid AWS regions (in AWS format with hyphens)
const VALID_REGIONS: &[&str] = &[
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-south-1",
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "ca-central-1",
    "sa-east-1",
];

/// AWS region type with custom validation
/// Accepts:
/// - DSL format: aws.Region.ap_northeast_1
/// - AWS string format: "ap-northeast-1"
pub fn aws_region() -> AttributeType {
    AttributeType::Custom {
        name: "Region".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                let normalized = normalize_region(s);
                if VALID_REGIONS.contains(&normalized.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid region '{}', expected one of: {} or DSL format like aws.Region.ap_northeast_1",
                        s,
                        VALID_REGIONS.join(", ")
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
    }
}

/// Normalize region string to AWS format (hyphens)
fn normalize_region(s: &str) -> String {
    let region_part = if s.contains('.') {
        s.split('.').next_back().unwrap_or(s)
    } else {
        s
    };
    region_part.replace('_', "-")
}

/// Valid instance tenancy values
const VALID_INSTANCE_TENANCY: &[&str] = &["default", "dedicated", "host"];

/// Instance tenancy type for VPC
/// Accepts:
/// - DSL format: awscc.vpc.InstanceTenancy.default
/// - String format: "default"
pub fn instance_tenancy() -> AttributeType {
    AttributeType::Custom {
        name: "InstanceTenancy".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                // Check namespace format if it contains dots
                if s.contains('.') {
                    // Must be awscc.vpc.InstanceTenancy.<value>
                    let parts: Vec<&str> = s.split('.').collect();
                    if parts.len() != 4
                        || parts[0] != "awscc"
                        || parts[1] != "vpc"
                        || parts[2] != "InstanceTenancy"
                    {
                        return Err(format!(
                            "Invalid instance tenancy '{}', expected one of: awscc.vpc.InstanceTenancy.default, awscc.vpc.InstanceTenancy.dedicated, awscc.vpc.InstanceTenancy.host",
                            s
                        ));
                    }
                }
                let normalized = normalize_instance_tenancy(s);
                if VALID_INSTANCE_TENANCY.contains(&normalized.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid instance tenancy '{}', expected one of: awscc.vpc.InstanceTenancy.default, awscc.vpc.InstanceTenancy.dedicated, awscc.vpc.InstanceTenancy.host",
                        s
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
    }
}

/// Normalize instance tenancy to API format
/// - "awscc.vpc.InstanceTenancy.default" -> "default"
/// - "default" -> "default"
pub fn normalize_instance_tenancy(s: &str) -> String {
    if s.contains('.') {
        s.split('.').next_back().unwrap_or(s).to_string()
    } else {
        s.to_string()
    }
}

/// Tags type for AWS resources (Terraform-style map)
/// Example: tags = { Environment = "production", Project = "myapp" }
pub fn tags_type() -> AttributeType {
    AttributeType::Map(Box::new(AttributeType::String))
}

/// Returns the schema for VPC (AWS Cloud Control)
///
/// Based on CloudFormation AWS::EC2::VPC resource type.
/// See: https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpc.html
pub fn vpc_schema() -> ResourceSchema {
    ResourceSchema::new("awscc.vpc")
        .with_description("An AWS VPC (Virtual Private Cloud) managed via Cloud Control API")
        // ========== Carina-specific attributes ==========
        .attribute(
            AttributeSchema::new("name", AttributeType::String)
                .required()
                .with_description("VPC name (Name tag) - Carina identifier"),
        )
        .attribute(
            AttributeSchema::new("region", aws_region()).with_description(
                "The AWS region for the VPC (inherited from provider if not specified)",
            ),
        )
        // ========== CloudFormation input properties ==========
        .attribute(
            AttributeSchema::new("cidr_block", types::cidr())
                .with_description("The IPv4 network range for the VPC, in CIDR notation. Required if not using Ipv4IpamPoolId."),
        )
        .attribute(
            AttributeSchema::new("enable_dns_hostnames", AttributeType::Bool)
                .with_description("Indicates whether instances launched in the VPC get DNS hostnames. Default: false"),
        )
        .attribute(
            AttributeSchema::new("enable_dns_support", AttributeType::Bool)
                .with_description("Indicates whether the DNS resolution is supported for the VPC. Default: true"),
        )
        .attribute(
            AttributeSchema::new("instance_tenancy", instance_tenancy())
                .with_description("The allowed tenancy of instances launched into the VPC. Values: default, dedicated, host")
                .with_completions(vec![
                    CompletionValue::new("awscc.vpc.InstanceTenancy.default", "Instances can have any tenancy"),
                    CompletionValue::new("awscc.vpc.InstanceTenancy.dedicated", "Instances run on single-tenant hardware"),
                    CompletionValue::new("awscc.vpc.InstanceTenancy.host", "Instances run on dedicated host"),
                ]),
        )
        .attribute(
            AttributeSchema::new("ipv4_ipam_pool_id", AttributeType::String)
                .with_description("The ID of an IPv4 IPAM pool to allocate the VPC CIDR from"),
        )
        .attribute(
            AttributeSchema::new("ipv4_netmask_length", AttributeType::Int)
                .with_description("The netmask length of the IPv4 CIDR to allocate from an IPAM pool"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags for the VPC (Terraform-style map). Example: { Environment = \"production\" }"),
        )
        // ========== CloudFormation return values (read-only) ==========
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .with_description("The ID of the VPC (read-only)"),
        )
        .attribute(
            AttributeSchema::new("cidr_block_associations", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("The association IDs of the IPv4 CIDR blocks for the VPC (read-only)"),
        )
        .attribute(
            AttributeSchema::new("default_network_acl", AttributeType::String)
                .with_description("The ID of the default network ACL for the VPC (read-only)"),
        )
        .attribute(
            AttributeSchema::new("default_security_group", AttributeType::String)
                .with_description("The ID of the default security group for the VPC (read-only)"),
        )
        .attribute(
            AttributeSchema::new("ipv6_cidr_blocks", AttributeType::List(Box::new(AttributeType::String)))
                .with_description("The IPv6 CIDR blocks associated with the VPC (read-only)"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn valid_vpc_minimal() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-vpc".to_string()));
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn valid_vpc_full() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-vpc".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("aws.Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        attrs.insert("enable_dns_support".to_string(), Value::Bool(true));
        attrs.insert("enable_dns_hostnames".to_string(), Value::Bool(true));
        attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("default".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn valid_vpc_with_ipam() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-vpc".to_string()));
        attrs.insert(
            "ipv4_ipam_pool_id".to_string(),
            Value::String("ipam-pool-0123456789abcdef0".to_string()),
        );
        attrs.insert("ipv4_netmask_length".to_string(), Value::Int(16));

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn vpc_missing_name() {
        let schema = vpc_schema();
        let mut attrs = HashMap::new();
        attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let result = schema.validate(&attrs);
        assert!(result.is_err());
    }

    #[test]
    fn valid_instance_tenancy_dsl_format() {
        let tenancy = instance_tenancy();
        assert!(
            tenancy
                .validate(&Value::String(
                    "awscc.vpc.InstanceTenancy.default".to_string()
                ))
                .is_ok()
        );
        assert!(
            tenancy
                .validate(&Value::String(
                    "awscc.vpc.InstanceTenancy.dedicated".to_string()
                ))
                .is_ok()
        );
        assert!(
            tenancy
                .validate(&Value::String("awscc.vpc.InstanceTenancy.host".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn valid_instance_tenancy_string_format() {
        let tenancy = instance_tenancy();
        assert!(
            tenancy
                .validate(&Value::String("default".to_string()))
                .is_ok()
        );
        assert!(
            tenancy
                .validate(&Value::String("dedicated".to_string()))
                .is_ok()
        );
        assert!(tenancy.validate(&Value::String("host".to_string())).is_ok());
    }

    #[test]
    fn invalid_instance_tenancy() {
        let tenancy = instance_tenancy();
        assert!(
            tenancy
                .validate(&Value::String("shared".to_string()))
                .is_err()
        );
    }

    #[test]
    fn invalid_instance_tenancy_wrong_namespace() {
        let tenancy = instance_tenancy();
        // Typo: awscc.vp instead of awscc.vpc
        assert!(
            tenancy
                .validate(&Value::String(
                    "awscc.vp.InstanceTenancy.default".to_string()
                ))
                .is_err()
        );
        // Wrong provider: aws instead of awscc
        assert!(
            tenancy
                .validate(&Value::String(
                    "aws.vpc.InstanceTenancy.default".to_string()
                ))
                .is_err()
        );
        // Wrong type name
        assert!(
            tenancy
                .validate(&Value::String("awscc.vpc.Tenancy.default".to_string()))
                .is_err()
        );
    }

    #[test]
    fn normalize_instance_tenancy_dsl_format() {
        assert_eq!(
            normalize_instance_tenancy("awscc.vpc.InstanceTenancy.default"),
            "default"
        );
        assert_eq!(
            normalize_instance_tenancy("awscc.vpc.InstanceTenancy.dedicated"),
            "dedicated"
        );
        assert_eq!(
            normalize_instance_tenancy("awscc.vpc.InstanceTenancy.host"),
            "host"
        );
    }

    #[test]
    fn normalize_instance_tenancy_string_format() {
        assert_eq!(normalize_instance_tenancy("default"), "default");
        assert_eq!(normalize_instance_tenancy("dedicated"), "dedicated");
    }

    #[test]
    fn region_accepts_aws_format() {
        let region_type = aws_region();
        assert!(
            region_type
                .validate(&Value::String("ap-northeast-1".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn region_accepts_dsl_format() {
        let region_type = aws_region();
        assert!(
            region_type
                .validate(&Value::String("aws.Region.ap_northeast_1".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn region_rejects_invalid_region() {
        let region_type = aws_region();
        assert!(
            region_type
                .validate(&Value::String("invalid-region".to_string()))
                .is_err()
        );
    }
}
