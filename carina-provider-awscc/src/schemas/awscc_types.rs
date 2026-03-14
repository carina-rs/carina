//! AWS Cloud Control type definitions and validators
//!
//! This module re-exports shared AWS type validators from `carina-aws-types`
//! and defines provider-specific types (region, availability zone, schema config,
//! IAM policy document).

pub use carina_aws_types::*;

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
                if VALID_REGIONS.contains(&normalized.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid region '{}', expected one of: {} or DSL format like awscc.Region.ap_northeast_1",
                        s,
                        VALID_REGIONS.join(", ")
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("awscc".to_string()),
        to_dsl: None,
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
