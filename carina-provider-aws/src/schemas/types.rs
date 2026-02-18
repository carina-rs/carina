//! AWS-specific type definitions

use carina_core::resource::Value;
use carina_core::schema::AttributeType;
use carina_core::utils::{extract_enum_value, validate_enum_namespace};

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
        namespace: Some("aws".to_string()),
        to_dsl: None,
    }
}

/// Valid versioning status values
const VALID_VERSIONING_STATUS: &[&str] = &["Enabled", "Suspended"];

/// S3 bucket versioning status
/// Accepts:
/// - DSL format: aws.s3.VersioningStatus.Enabled
/// - Short DSL format: VersioningStatus.Enabled
/// - Value only: Enabled, Suspended
pub fn versioning_status() -> AttributeType {
    AttributeType::Custom {
        name: "VersioningStatus".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_enum_namespace(s, "VersioningStatus", "aws.s3")
                    .map_err(|reason| format!("Invalid versioning status '{}': {}", s, reason))?;
                let normalized = extract_enum_value(s);
                if VALID_VERSIONING_STATUS.contains(&normalized) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid versioning status '{}', expected one of: Enabled, Suspended",
                        s
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: Some("aws.s3".to_string()),
        to_dsl: None,
    }
}

/// S3 ACL enum type
pub fn s3_acl() -> AttributeType {
    AttributeType::Enum(vec![
        "private".to_string(),
        "public_read".to_string(),
        "public_read_write".to_string(),
        "authenticated_read".to_string(),
    ])
}

/// S3 bucket name type (with validation)
pub fn s3_bucket_name() -> AttributeType {
    AttributeType::Custom {
        name: "BucketName".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                if s.len() < 3 {
                    return Err("Bucket name must be at least 3 characters".to_string());
                }
                if s.len() > 63 {
                    return Err("Bucket name must be at most 63 characters".to_string());
                }
                if !s.chars().next().unwrap_or('_').is_ascii_lowercase()
                    && !s.chars().next().unwrap_or('_').is_ascii_digit()
                {
                    return Err(
                        "Bucket name must start with a lowercase letter or number".to_string()
                    );
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
        for region in VALID_REGIONS {
            assert!(
                region_type
                    .validate(&Value::String(region.to_string()))
                    .is_ok(),
                "Region {} should be valid",
                region
            );
        }
    }

    #[test]
    fn region_rejects_wrong_namespace() {
        let region_type = aws_region();
        // Wrong provider prefix
        assert!(
            region_type
                .validate(&Value::String("gcp.Region.ap_northeast_1".to_string()))
                .is_err()
        );
        // Wrong type name
        assert!(
            region_type
                .validate(&Value::String("aws.Location.ap_northeast_1".to_string()))
                .is_err()
        );
        // Too many parts
        assert!(
            region_type
                .validate(&Value::String("foo.bar.baz.ap_northeast_1".to_string()))
                .is_err()
        );
        // 2-part with wrong type name
        assert!(
            region_type
                .validate(&Value::String("Location.ap_northeast_1".to_string()))
                .is_err()
        );
    }

    // Versioning status tests

    #[test]
    fn versioning_accepts_dsl_format_enabled() {
        let versioning = versioning_status();
        assert!(
            versioning
                .validate(&Value::String(
                    "aws.s3.VersioningStatus.Enabled".to_string()
                ))
                .is_ok()
        );
    }

    #[test]
    fn versioning_accepts_dsl_format_suspended() {
        let versioning = versioning_status();
        assert!(
            versioning
                .validate(&Value::String(
                    "aws.s3.VersioningStatus.Suspended".to_string()
                ))
                .is_ok()
        );
    }

    #[test]
    fn versioning_accepts_string_enabled() {
        let versioning = versioning_status();
        assert!(
            versioning
                .validate(&Value::String("Enabled".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn versioning_accepts_string_suspended() {
        let versioning = versioning_status();
        assert!(
            versioning
                .validate(&Value::String("Suspended".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn versioning_rejects_lowercase() {
        let versioning = versioning_status();
        assert!(
            versioning
                .validate(&Value::String("enabled".to_string()))
                .is_err()
        );
    }

    #[test]
    fn versioning_rejects_bool() {
        let versioning = versioning_status();
        assert!(versioning.validate(&Value::Bool(true)).is_err());
        assert!(versioning.validate(&Value::Bool(false)).is_err());
    }

    #[test]
    fn versioning_rejects_disabled() {
        let versioning = versioning_status();
        assert!(
            versioning
                .validate(&Value::String("Disabled".to_string()))
                .is_err()
        );
    }

    #[test]
    fn versioning_rejects_wrong_namespace() {
        let versioning = versioning_status();
        // Typo: aws.s.VersioningStatus instead of aws.s3.VersioningStatus
        assert!(
            versioning
                .validate(&Value::String("aws.s.VersioningStatus.Enabled".to_string()))
                .is_err()
        );
        // Wrong provider
        assert!(
            versioning
                .validate(&Value::String(
                    "awscc.s3.VersioningStatus.Enabled".to_string()
                ))
                .is_err()
        );
        // Wrong type name
        assert!(
            versioning
                .validate(&Value::String("aws.s3.Versioning.Enabled".to_string()))
                .is_err()
        );
    }
}
