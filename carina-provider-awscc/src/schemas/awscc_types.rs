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

/// Register AWSCC type validators declaratively.
///
/// Generates a `HashMap<String, ValidatorFn>` from three categories:
/// - `simple`: single-arg validators (`name => function`)
/// - `prefixed`: prefixed resource ID validators (`name => prefix`)
/// - `service_arn`: service ARN validators (`name => (service, resource_prefix)`)
macro_rules! register_validators {
    (
        simple { $( $s_name:ident => $s_fn:expr ),* $(,)? }
        prefixed { $( $p_name:ident => $p_prefix:expr ),* $(,)? }
        service_arn { $( $a_name:ident => $a_expr:expr ),* $(,)? }
    ) => {{
        let mut m: HashMap<String, ValidatorFn> = HashMap::new();
        $( m.insert(stringify!($s_name).to_string(), Box::new(|s: &str| ($s_fn)(s))); )*
        $( m.insert(stringify!($p_name).to_string(), Box::new(|s: &str| validate_prefixed_resource_id(s, $p_prefix))); )*
        $( m.insert(stringify!($a_name).to_string(), Box::new($a_expr)); )*
        m
    }};
}

/// Return all AWSCC type validators for registration in ProviderContext.
///
/// These validators are keyed by type name (matching the names used in fn/module
/// type annotations) and wrap the validation functions from `carina-aws-types`.
pub fn awscc_validators() -> HashMap<String, ValidatorFn> {
    register_validators! {
        simple {
            arn => validate_arn,
            availability_zone => validate_availability_zone,
            aws_resource_id => validate_aws_resource_id,
            iam_role_id => validate_iam_role_id,
            aws_account_id => validate_aws_account_id,
            kms_key_id => validate_kms_key_id,
            ipam_pool_id => validate_ipam_pool_id,
            availability_zone_id => validate_availability_zone_id,
        }
        prefixed {
            vpc_id => "vpc",
            subnet_id => "subnet",
            security_group_id => "sg",
            internet_gateway_id => "igw",
            route_table_id => "rtb",
            nat_gateway_id => "nat",
            transit_gateway_id => "tgw",
            vpn_gateway_id => "vgw",
            network_interface_id => "eni",
            allocation_id => "eipalloc",
            vpc_endpoint_id => "vpce",
            vpc_peering_connection_id => "pcx",
            instance_id => "i",
            prefix_list_id => "pl",
            carrier_gateway_id => "cagw",
            local_gateway_id => "lgw",
            network_acl_id => "acl",
            transit_gateway_attachment_id => "tgw-attach",
            flow_log_id => "fl",
            ipam_id => "ipam",
            subnet_route_table_association_id => "rtbassoc",
            security_group_rule_id => "sgr",
            vpc_cidr_block_association_id => "vpc-cidr-assoc",
            tgw_route_table_id => "tgw-rtb",
            egress_only_internet_gateway_id => "eigw",
        }
        service_arn {
            iam_role_arn => |s: &str| validate_service_arn(s, "iam", Some("role/")),
            iam_policy_arn => |s: &str| validate_service_arn(s, "iam", Some("policy/")),
            kms_key_arn => |s: &str| validate_kms_key_id(s),
        }
    }
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
    fn awscc_validators_all_registered() {
        let validators = awscc_validators();

        // Single-arg validators
        let expected_single = [
            "arn",
            "availability_zone",
            "aws_resource_id",
            "iam_role_id",
            "aws_account_id",
            "kms_key_id",
            "ipam_pool_id",
            "availability_zone_id",
        ];

        // Prefixed resource IDs
        let expected_prefixed = [
            "vpc_id",
            "subnet_id",
            "security_group_id",
            "internet_gateway_id",
            "route_table_id",
            "nat_gateway_id",
            "transit_gateway_id",
            "vpn_gateway_id",
            "network_interface_id",
            "allocation_id",
            "vpc_endpoint_id",
            "vpc_peering_connection_id",
            "instance_id",
            "prefix_list_id",
            "carrier_gateway_id",
            "local_gateway_id",
            "network_acl_id",
            "transit_gateway_attachment_id",
            "flow_log_id",
            "ipam_id",
            "subnet_route_table_association_id",
            "security_group_rule_id",
            "vpc_cidr_block_association_id",
            "tgw_route_table_id",
            "egress_only_internet_gateway_id",
        ];

        // Service ARNs
        let expected_arn = ["iam_role_arn", "iam_policy_arn", "kms_key_arn"];

        let mut all_expected: Vec<&str> = Vec::new();
        all_expected.extend_from_slice(&expected_single);
        all_expected.extend_from_slice(&expected_prefixed);
        all_expected.extend_from_slice(&expected_arn);

        for name in &all_expected {
            assert!(
                validators.contains_key(*name),
                "Missing validator: {}",
                name
            );
        }

        assert_eq!(
            validators.len(),
            all_expected.len(),
            "Validator count mismatch: expected {}, got {}. Extra keys: {:?}",
            all_expected.len(),
            validators.len(),
            validators
                .keys()
                .filter(|k| !all_expected.contains(&k.as_str()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn awscc_validators_produce_correct_results() {
        let validators = awscc_validators();

        // Test a prefixed resource ID validator
        let vpc_validator = validators.get("vpc_id").unwrap();
        assert!(vpc_validator("vpc-12345678").is_ok());
        assert!(vpc_validator("subnet-12345678").is_err());

        // Test a single-arg validator
        let arn_validator = validators.get("arn").unwrap();
        assert!(arn_validator("arn:aws:s3:::my-bucket").is_ok());
        assert!(arn_validator("not-an-arn").is_err());

        // Test a service ARN validator
        let iam_role_arn_validator = validators.get("iam_role_arn").unwrap();
        assert!(iam_role_arn_validator("arn:aws:iam::123456789012:role/my-role").is_ok());
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
