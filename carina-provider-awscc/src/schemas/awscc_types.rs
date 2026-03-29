//! AWS Cloud Control type definitions and validators
//!
//! This module re-exports shared AWS type validators from `carina-aws-types`
//! and defines provider-specific types (region, availability zone, schema config,
//! IAM policy document).

pub use carina_aws_types::*;

use std::collections::HashMap;

use carina_core::parser::ValidatorFn;
use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema};
use carina_core::utils::{extract_enum_value, validate_enum_namespace};

/// AWS Cloud Control schema configuration
///
/// Combines the generated ResourceSchema with AWS-specific metadata
/// that was previously in ResourceConfig.
pub struct AwsccSchemaConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Resource type name used in DSL (e.g., "ec2.vpc")
    pub resource_type_name: &'static str,
    /// Whether this resource type uses tags
    pub has_tags: bool,
    /// The resource schema with attribute definitions
    pub schema: ResourceSchema,
}

/// Return all AWSCC type validators for registration in ProviderContext.
///
/// These validators are keyed by type name (matching the names used in fn/module
/// type annotations) and wrap the validation functions from `carina-aws-types`.
pub fn awscc_validators() -> HashMap<String, ValidatorFn> {
    let mut m: HashMap<String, ValidatorFn> = HashMap::new();

    // Single-arg validators
    m.insert("arn".to_string(), Box::new(|s: &str| validate_arn(s)));
    m.insert(
        "availability_zone".to_string(),
        Box::new(|s: &str| validate_availability_zone(s)),
    );
    m.insert(
        "aws_resource_id".to_string(),
        Box::new(|s: &str| validate_aws_resource_id(s)),
    );
    m.insert(
        "iam_role_id".to_string(),
        Box::new(|s: &str| validate_iam_role_id(s)),
    );
    m.insert(
        "aws_account_id".to_string(),
        Box::new(|s: &str| validate_aws_account_id(s)),
    );
    m.insert(
        "kms_key_id".to_string(),
        Box::new(|s: &str| validate_kms_key_id(s)),
    );
    m.insert(
        "ipam_pool_id".to_string(),
        Box::new(|s: &str| validate_ipam_pool_id(s)),
    );
    m.insert(
        "availability_zone_id".to_string(),
        Box::new(|s: &str| validate_availability_zone_id(s)),
    );

    // Prefixed resource IDs
    m.insert(
        "vpc_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "vpc")),
    );
    m.insert(
        "subnet_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "subnet")),
    );
    m.insert(
        "security_group_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "sg")),
    );
    m.insert(
        "internet_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "igw")),
    );
    m.insert(
        "route_table_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "rtb")),
    );
    m.insert(
        "nat_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "nat")),
    );
    m.insert(
        "transit_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "tgw")),
    );
    m.insert(
        "vpn_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "vgw")),
    );
    m.insert(
        "network_interface_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "eni")),
    );
    m.insert(
        "allocation_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "eipalloc")),
    );
    m.insert(
        "vpc_endpoint_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "vpce")),
    );
    m.insert(
        "vpc_peering_connection_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "pcx")),
    );
    m.insert(
        "instance_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "i")),
    );
    m.insert(
        "prefix_list_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "pl")),
    );
    m.insert(
        "carrier_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "cagw")),
    );
    m.insert(
        "local_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "lgw")),
    );
    m.insert(
        "network_acl_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "acl")),
    );
    m.insert(
        "transit_gateway_attachment_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "tgw-attach")),
    );
    m.insert(
        "flow_log_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "fl")),
    );
    m.insert(
        "ipam_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "ipam")),
    );
    m.insert(
        "subnet_route_table_association_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "rtbassoc")),
    );
    m.insert(
        "security_group_rule_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "sgr")),
    );
    m.insert(
        "vpc_cidr_block_association_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "vpc-cidr-assoc")),
    );
    m.insert(
        "tgw_route_table_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "tgw-rtb")),
    );
    m.insert(
        "egress_only_internet_gateway_id".to_string(),
        Box::new(|s: &str| validate_prefixed_resource_id(s, "eigw")),
    );

    // Service ARNs
    m.insert(
        "iam_role_arn".to_string(),
        Box::new(|s: &str| validate_service_arn(s, "iam", Some("role/"))),
    );
    m.insert(
        "iam_policy_arn".to_string(),
        Box::new(|s: &str| validate_service_arn(s, "iam", Some("policy/"))),
    );
    m.insert(
        "kms_key_arn".to_string(),
        Box::new(|s: &str| validate_kms_key_id(s)),
    );

    m
}

/// Validate a namespaced enum value.
/// Returns Ok(()) if valid, Err with bare reason string if invalid.
/// Callers are responsible for adding context (e.g., what value was provided).
#[cfg(test)]
pub(crate) fn validate_namespaced_enum(
    value: &Value,
    type_name: &str,
    namespace: &str,
    valid_values: &[&str],
) -> Result<(), String> {
    if let Value::String(s) = value {
        validate_enum_namespace(s, type_name, namespace)?;

        let normalized = extract_enum_value(s);
        if find_matching_enum_value(normalized, valid_values).is_some() {
            Ok(())
        } else {
            Err(format!("expected one of: {}", valid_values.join(", ")))
        }
    } else {
        Err("Expected string".to_string())
    }
}

/// AWSCC region type with custom validation
/// Accepts:
/// - DSL format: awscc.Region.ap_northeast_1
/// - AWS string format: "ap-northeast-1"
/// - Shorthand: ap_northeast_1
pub fn awscc_region() -> AttributeType {
    AttributeType::Custom {
        name: "Region".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "Region", "awscc")
                    .map_err(|reason| format!("Invalid region '{}': {}", s, reason))?;
                let normalized = extract_enum_value(s).replace('_', "-");
                if is_valid_region(&normalized) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid region '{}', expected one of: {} or DSL format like awscc.Region.ap_northeast_1",
                        s,
                        valid_regions_display()
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("awscc".to_string()),
        to_dsl: Some(|s: &str| s.replace('-', "_")),
    }
}

/// Availability Zone type (e.g., "us-east-1a", "ap-northeast-1c")
/// Validates format: region + single letter zone identifier
pub fn availability_zone() -> AttributeType {
    AttributeType::Custom {
        name: "AvailabilityZone".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "AvailabilityZone", "awscc")
                    .map_err(|reason| format!("Invalid availability zone '{}': {}", s, reason))?;
                let extracted = extract_enum_value(s);
                let normalized = extracted.replace('_', "-");
                validate_availability_zone(&normalized)
                    .map_err(|reason| format!("Invalid availability zone '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("awscc".to_string()),
        to_dsl: Some(|s: &str| s.replace('-', "_")),
    }
}

// iam_policy_document() and validate_iam_policy_document() are provided by
// `pub use carina_aws_types::*` above

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_availability_zone_namespace_expanded() {
        let t = availability_zone();
        // Full namespace format
        assert!(
            t.validate(&Value::String(
                "awscc.AvailabilityZone.us_east_1a".to_string()
            ))
            .is_ok()
        );
        // Type.value format
        assert!(
            t.validate(&Value::String("AvailabilityZone.us_east_1a".to_string()))
                .is_ok()
        );
        // Shorthand format
        assert!(t.validate(&Value::String("us_east_1a".to_string())).is_ok());
        // AWS format
        assert!(t.validate(&Value::String("us-east-1a".to_string())).is_ok());
    }

    #[test]
    fn validate_availability_zone_rejects_wrong_namespace() {
        let t = availability_zone();
        assert!(
            t.validate(&Value::String(
                "aws.AvailabilityZone.us_east_1a".to_string()
            ))
            .is_err()
        );
    }

    #[test]
    fn validate_availability_zone_rejects_invalid() {
        let t = availability_zone();
        assert!(t.validate(&Value::String("us-east-1".to_string())).is_err()); // no zone letter
        assert!(t.validate(&Value::String("invalid".to_string())).is_err());
    }

    #[test]
    fn validate_availability_zone_to_dsl() {
        let t = availability_zone();
        if let AttributeType::Custom { to_dsl, .. } = &t {
            let f = to_dsl.unwrap();
            assert_eq!(f("us-east-1a"), "us_east_1a");
            assert_eq!(f("ap-northeast-1c"), "ap_northeast_1c");
        } else {
            panic!("Expected Custom type");
        }
    }

    #[test]
    fn awscc_region_accepts_awscc_namespace() {
        let region_type = awscc_region();
        assert!(
            region_type
                .validate(&Value::String("awscc.Region.ap_northeast_1".to_string()))
                .is_ok()
        );
        assert!(
            region_type
                .validate(&Value::String("ap-northeast-1".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn awscc_region_rejects_aws_namespace() {
        let region_type = awscc_region();
        assert!(
            region_type
                .validate(&Value::String("aws.Region.ap_northeast_1".to_string()))
                .is_err()
        );
    }

    #[test]
    fn validate_namespaced_enum_basic() {
        let result = validate_namespaced_enum(
            &Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
            "InstanceTenancy",
            "awscc.ec2.vpc",
            &["default", "dedicated", "host"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_iam_policy_document_basic() {
        let doc = Value::Map(
            vec![
                (
                    "version".to_string(),
                    Value::String("2012-10-17".to_string()),
                ),
                (
                    "statement".to_string(),
                    Value::List(vec![Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            (
                                "action".to_string(),
                                Value::String("sts:AssumeRole".to_string()),
                            ),
                            ("resource".to_string(), Value::String("*".to_string())),
                        ]
                        .into_iter()
                        .collect(),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        assert!(validate_iam_policy_document(&doc).is_ok());
    }
}
