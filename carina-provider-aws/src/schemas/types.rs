//! AWS-specific type definitions and validators
//!
//! This module re-exports shared AWS type validators from `carina-aws-types`
//! and defines provider-specific types (region, availability zone, schema config).

pub use carina_aws_types::*;

use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema};
use carina_core::utils::{extract_enum_value, validate_enum_namespace};

/// AWS schema configuration
///
/// Combines the generated ResourceSchema with AWS-specific metadata.
pub struct AwsSchemaConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Resource type name used in DSL (e.g., "ec2_vpc")
    pub resource_type_name: &'static str,
    /// Whether this resource type uses tags
    pub has_tags: bool,
    /// The resource schema with attribute definitions
    pub schema: ResourceSchema,
}

/// AWS region type with custom validation
/// Accepts:
/// - DSL format: aws.Region.ap_northeast_1
/// - AWS string format: "ap-northeast-1"
/// - Shorthand: ap_northeast_1
pub fn aws_region() -> AttributeType {
    AttributeType::Custom {
        name: "Region".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "Region", "aws")
                    .map_err(|reason| format!("Invalid region '{}': {}", s, reason))?;
                // Normalize the input to AWS format (hyphens)
                let normalized = extract_enum_value(s).replace('_', "-");
                if is_valid_region(&normalized) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid region '{}', expected one of: {} or DSL format like aws.Region.ap_northeast_1",
                        s,
                        valid_regions_display()
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("aws".to_string()),
        to_dsl: Some(|s: &str| s.replace('-', "_")),
    }
}

/// Availability zone type with validation (e.g., "us-east-1a")
/// Accepts:
/// - DSL format: aws.AvailabilityZone.us_east_1a
/// - AWS string format: "us-east-1a"
/// - Shorthand: us_east_1a
pub fn availability_zone() -> AttributeType {
    AttributeType::Custom {
        name: "AvailabilityZone".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "AvailabilityZone", "aws")
                    .map_err(|reason| format!("Invalid availability zone '{}': {}", s, reason))?;
                let extracted = extract_enum_value(s);
                let normalized = extracted.replace('_', "-");
                validate_availability_zone(&normalized)
                    .map_err(|reason| format!("Invalid availability zone '{}': {}", s, reason))
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("aws".to_string()),
        to_dsl: Some(|s: &str| s.replace('-', "_")),
    }
}

/// S3 grantee specification type with validation
///
/// Validates that the value contains at least one grantee spec in the format:
/// - `id="canonical-user-id"`
/// - `emailAddress="user@example.com"`
/// - `uri="http://acs.amazonaws.com/groups/global/AllUsers"`
///
/// Multiple grantees can be comma-separated.
pub fn s3_grantee() -> AttributeType {
    AttributeType::Custom {
        name: "S3Grantee".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                if s.is_empty() {
                    return Err("Grantee specification must not be empty".to_string());
                }
                // Split by comma and validate each grantee spec
                for part in s.split(',') {
                    let trimmed = part.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let valid_prefixes = ["id=", "emailAddress=", "uri="];
                    if !valid_prefixes.iter().any(|p| trimmed.starts_with(p)) {
                        return Err(format!(
                            "Invalid grantee spec '{}': must start with id=, emailAddress=, or uri=",
                            trimmed
                        ));
                    }
                }
                Ok(())
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    }
}

// iam_policy_document() is provided by `pub use carina_aws_types::*` above

#[cfg(test)]
mod tests {
    use super::*;

    // Region validation tests

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
    fn region_accepts_dsl_format_without_aws_prefix() {
        let region_type = aws_region();
        assert!(
            region_type
                .validate(&Value::String("Region.ap_northeast_1".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn region_rejects_invalid_region() {
        let region_type = aws_region();
        let result = region_type.validate(&Value::String("invalid-region".to_string()));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid region"));
        assert!(err.contains("ap-northeast-1")); // Should suggest valid regions
    }

    #[test]
    fn region_rejects_availability_zone() {
        let region_type = aws_region();
        // ap-northeast-1a is an AZ, not a region
        assert!(
            region_type
                .validate(&Value::String("ap-northeast-1a".to_string()))
                .is_err()
        );
    }

    #[test]
    fn region_validates_all_valid_regions() {
        let region_type = aws_region();
        for (region, _) in REGIONS {
            assert!(
                region_type
                    .validate(&Value::String(region.to_string()))
                    .is_ok(),
                "Region {} should be valid",
                region
            );
        }
    }

    // Availability zone validation tests

    #[test]
    fn az_accepts_aws_format() {
        let az_type = availability_zone();
        assert!(
            az_type
                .validate(&Value::String("us-east-1a".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn az_accepts_dsl_format() {
        let az_type = availability_zone();
        assert!(
            az_type
                .validate(&Value::String(
                    "aws.AvailabilityZone.us_east_1a".to_string()
                ))
                .is_ok()
        );
    }

    #[test]
    fn az_accepts_shorthand_format() {
        let az_type = availability_zone();
        assert!(
            az_type
                .validate(&Value::String("us_east_1a".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn az_rejects_invalid_az() {
        let az_type = availability_zone();
        assert!(
            az_type
                .validate(&Value::String("invalid-zone".to_string()))
                .is_err()
        );
    }

    #[test]
    fn az_rejects_wrong_namespace() {
        let az_type = availability_zone();
        assert!(
            az_type
                .validate(&Value::String(
                    "gcp.AvailabilityZone.us_east_1a".to_string()
                ))
                .is_err()
        );
    }

    #[test]
    fn az_has_namespace() {
        let az_type = availability_zone();
        if let AttributeType::Custom { namespace, .. } = &az_type {
            assert_eq!(namespace.as_deref(), Some("aws"));
        } else {
            panic!("Expected Custom type");
        }
    }

    #[test]
    fn az_has_to_dsl() {
        let az_type = availability_zone();
        if let AttributeType::Custom { to_dsl, .. } = &az_type {
            assert!(to_dsl.is_some());
            let convert = to_dsl.unwrap();
            assert_eq!(convert("us-east-1a"), "us_east_1a");
        } else {
            panic!("Expected Custom type");
        }
    }

    // S3 grantee validation tests

    #[test]
    fn grantee_accepts_id_format() {
        let t = s3_grantee();
        assert!(
            t.validate(&Value::String(
                "id=\"79a59df900b949e55d96a1e698fbacedfd6e09d98eacf8f8d5218e7cd47ef2be\""
                    .to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn grantee_accepts_email_format() {
        let t = s3_grantee();
        assert!(
            t.validate(&Value::String(
                "emailAddress=\"user@example.com\"".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn grantee_accepts_uri_format() {
        let t = s3_grantee();
        assert!(
            t.validate(&Value::String(
                "uri=\"http://acs.amazonaws.com/groups/global/AllUsers\"".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn grantee_accepts_multiple_specs() {
        let t = s3_grantee();
        assert!(
            t.validate(&Value::String(
                "id=\"abc123\", emailAddress=\"user@example.com\"".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn grantee_rejects_empty_string() {
        let t = s3_grantee();
        assert!(t.validate(&Value::String("".to_string())).is_err());
    }

    #[test]
    fn grantee_rejects_invalid_prefix() {
        let t = s3_grantee();
        let result = t.validate(&Value::String("foo=\"bar\"".to_string()));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must start with id=, emailAddress=, or uri="));
    }

    #[test]
    fn region_rejects_wrong_namespace() {
        let region_type = aws_region();
        assert!(
            region_type
                .validate(&Value::String("gcp.Region.ap_northeast_1".to_string()))
                .is_err()
        );
        assert!(
            region_type
                .validate(&Value::String("aws.Location.ap_northeast_1".to_string()))
                .is_err()
        );
        assert!(
            region_type
                .validate(&Value::String("foo.bar.baz.ap_northeast_1".to_string()))
                .is_err()
        );
        assert!(
            region_type
                .validate(&Value::String("Location.ap_northeast_1".to_string()))
                .is_err()
        );
    }
}
