//! Shared utility functions for value normalization and conversion

/// Extract the last dot-separated part from a namespaced identifier.
/// Returns the original string if no dots are present.
///
/// # Examples
///
/// ```
/// use carina_core::utils::extract_enum_value;
///
/// assert_eq!(extract_enum_value("aws.Region.ap_northeast_1"), "ap_northeast_1");
/// assert_eq!(extract_enum_value("aws.s3.VersioningStatus.Enabled"), "Enabled");
/// assert_eq!(extract_enum_value("Enabled"), "Enabled");
/// ```
pub fn extract_enum_value(s: &str) -> &str {
    if s.contains('.') {
        s.split('.').next_back().unwrap_or(s)
    } else {
        s
    }
}

/// Convert DSL enum value to provider SDK format.
///
/// Handles the following patterns:
/// - 2-part: `TypeName.value_name` -> `value-name`
/// - 3-part: `provider.TypeName.value_name` -> `value-name`
/// - 4-part: `provider.resource.TypeName.value_name` -> `value-name`
///
/// The first component of TypeName must be uppercase.
/// Underscores in the extracted value are replaced with hyphens.
/// Returns the original value unchanged if it doesn't match any pattern.
///
/// # Examples
///
/// ```
/// use carina_core::utils::convert_enum_value;
///
/// assert_eq!(convert_enum_value("aws.Region.ap_northeast_1"), "ap-northeast-1");
/// assert_eq!(convert_enum_value("Region.ap_northeast_1"), "ap-northeast-1");
/// assert_eq!(convert_enum_value("awscc.ec2_ipam.Tier.advanced"), "advanced");
/// assert_eq!(convert_enum_value("eu-west-1"), "eu-west-1");
/// ```
pub fn convert_enum_value(value: &str) -> String {
    let parts: Vec<&str> = value.split('.').collect();
    let raw_value = match parts.len() {
        2 => {
            // TypeName.value pattern
            if parts[0].chars().next().is_some_and(|c| c.is_uppercase()) {
                parts[1]
            } else {
                return value.to_string();
            }
        }
        3 => {
            // provider.TypeName.value pattern
            let provider = parts[0];
            let type_name = parts[1];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                parts[2]
            } else {
                return value.to_string();
            }
        }
        // 4-part: provider.resource.TypeName.value
        // e.g., "awscc.ec2_ipam.Tier.advanced" -> "advanced"
        4 => {
            let provider = parts[0];
            let type_name = parts[2];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                parts[3]
            } else {
                return value.to_string();
            }
        }
        _ => return value.to_string(),
    };
    raw_value.replace('_', "-")
}

/// Validate namespace format for an enum identifier.
///
/// Handles the following formats:
/// - No dots: passes through (not a namespaced identifier)
/// - `TypeName.value` (2-part): validates that the first part matches `type_name`
/// - Full namespaced form (N-part): validates namespace segments and type_name
///
/// The expected full form length is determined by the namespace:
/// - `"aws"` (1 segment) → 3 parts: `aws.Region.value`
/// - `"aws.s3"` (2 segments) → 4 parts: `aws.s3.VersioningStatus.value`
///
/// # Arguments
/// * `s` - The input string to validate
/// * `type_name` - Expected type name (e.g., `"Region"`, `"InstanceTenancy"`)
/// * `namespace` - Expected namespace prefix (e.g., `"aws"`, `"aws.s3"`, `"awscc.ec2_vpc"`)
///
/// # Returns
/// * `Ok(())` if namespace is valid or string has no dots
/// * `Err(String)` with descriptive message if namespace is invalid
///
/// # Examples
///
/// ```
/// use carina_core::utils::validate_enum_namespace;
///
/// // No dots — passes through
/// assert!(validate_enum_namespace("Enabled", "VersioningStatus", "aws.s3").is_ok());
///
/// // 2-part: TypeName.value
/// assert!(validate_enum_namespace("Region.ap_northeast_1", "Region", "aws").is_ok());
/// assert!(validate_enum_namespace("Location.ap_northeast_1", "Region", "aws").is_err());
///
/// // Full namespaced form
/// assert!(validate_enum_namespace("aws.Region.ap_northeast_1", "Region", "aws").is_ok());
/// assert!(validate_enum_namespace("aws.s3.VersioningStatus.Enabled", "VersioningStatus", "aws.s3").is_ok());
/// ```
pub fn validate_enum_namespace(s: &str, type_name: &str, namespace: &str) -> Result<(), String> {
    if !s.contains('.') {
        return Ok(());
    }

    let parts: Vec<&str> = s.split('.').collect();
    let ns_parts: Vec<&str> = namespace.split('.').collect();
    let expected_full_len = ns_parts.len() + 2; // namespace segments + type_name + value

    match parts.len() {
        // 2-part: TypeName.value
        2 => {
            if parts[0] != type_name {
                return Err(format!(
                    "Invalid format '{}', expected {}.value or {}.{}.value",
                    s, type_name, namespace, type_name
                ));
            }
        }
        n if n == expected_full_len => {
            // Full namespaced form: namespace.TypeName.value
            for (i, &expected) in ns_parts.iter().enumerate() {
                if parts[i] != expected {
                    return Err(format!(
                        "Invalid format '{}', expected {}.{}.value",
                        s, namespace, type_name
                    ));
                }
            }
            if parts[ns_parts.len()] != type_name {
                return Err(format!(
                    "Invalid format '{}', expected {}.{}.value",
                    s, namespace, type_name
                ));
            }
        }
        _ => {
            return Err(format!(
                "Invalid format '{}', expected one of: value, {}.value, or {}.{}.value",
                s, type_name, namespace, type_name
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_enum_value_with_dots() {
        assert_eq!(
            extract_enum_value("aws.Region.ap_northeast_1"),
            "ap_northeast_1"
        );
        assert_eq!(
            extract_enum_value("aws.s3.VersioningStatus.Enabled"),
            "Enabled"
        );
        assert_eq!(
            extract_enum_value("aws.vpc.InstanceTenancy.default"),
            "default"
        );
        assert_eq!(extract_enum_value("InstanceTenancy.dedicated"), "dedicated");
    }

    #[test]
    fn test_extract_enum_value_without_dots() {
        assert_eq!(extract_enum_value("Enabled"), "Enabled");
        assert_eq!(extract_enum_value("default"), "default");
        assert_eq!(extract_enum_value("ap-northeast-1"), "ap-northeast-1");
    }

    #[test]
    fn test_convert_enum_value_2_part() {
        assert_eq!(
            convert_enum_value("Region.ap_northeast_1"),
            "ap-northeast-1"
        );
    }

    #[test]
    fn test_convert_enum_value_3_part() {
        assert_eq!(
            convert_enum_value("aws.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_enum_value("aws.Region.us_east_1"), "us-east-1");
        assert_eq!(
            convert_enum_value("aws.AvailabilityZone.ap_northeast_1a"),
            "ap-northeast-1a"
        );
        assert_eq!(
            convert_enum_value("aws.AvailabilityZone.us_east_1b"),
            "us-east-1b"
        );
    }

    #[test]
    fn test_convert_enum_value_4_part() {
        assert_eq!(
            convert_enum_value("awscc.ec2_ipam.Tier.advanced"),
            "advanced"
        );
        assert_eq!(
            convert_enum_value("awscc.ec2_ipam_pool.AddressFamily.IPv4"),
            "IPv4"
        );
        assert_eq!(
            convert_enum_value("awscc.ec2_vpc.InstanceTenancy.default"),
            "default"
        );
    }

    #[test]
    fn test_convert_enum_value_passthrough() {
        // Already in SDK format - should be returned unchanged
        assert_eq!(convert_enum_value("eu-west-1"), "eu-west-1");
        assert_eq!(convert_enum_value("ap-northeast-1a"), "ap-northeast-1a");
    }

    #[test]
    fn test_convert_enum_value_invalid_patterns() {
        // lowercase first part in 2-part -> not a TypeName pattern
        assert_eq!(convert_enum_value("region.us_east_1"), "region.us_east_1");
        // single value -> passthrough
        assert_eq!(convert_enum_value("Enabled"), "Enabled");
    }

    // validate_enum_namespace tests

    #[test]
    fn test_validate_namespace_no_dots() {
        // Plain values pass through without validation
        assert!(validate_enum_namespace("Enabled", "VersioningStatus", "aws.s3").is_ok());
        assert!(validate_enum_namespace("ap-northeast-1", "Region", "aws").is_ok());
        assert!(validate_enum_namespace("default", "InstanceTenancy", "aws.vpc").is_ok());
    }

    #[test]
    fn test_validate_namespace_2_part_valid() {
        assert!(validate_enum_namespace("Region.ap_northeast_1", "Region", "aws").is_ok());
        assert!(
            validate_enum_namespace("VersioningStatus.Enabled", "VersioningStatus", "aws.s3")
                .is_ok()
        );
        assert!(
            validate_enum_namespace("InstanceTenancy.default", "InstanceTenancy", "aws.vpc")
                .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "InstanceTenancy.default",
                "InstanceTenancy",
                "awscc.ec2_vpc"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_2_part_invalid() {
        assert!(validate_enum_namespace("Location.ap_northeast_1", "Region", "aws").is_err());
        assert!(
            validate_enum_namespace("Versioning.Enabled", "VersioningStatus", "aws.s3").is_err()
        );
        assert!(validate_enum_namespace("Tenancy.default", "InstanceTenancy", "aws.vpc").is_err());
    }

    #[test]
    fn test_validate_namespace_3_part_valid() {
        // 3-part is valid for 1-segment namespace (e.g., "aws")
        assert!(validate_enum_namespace("aws.Region.ap_northeast_1", "Region", "aws").is_ok());
    }

    #[test]
    fn test_validate_namespace_3_part_invalid() {
        // Wrong provider
        assert!(validate_enum_namespace("gcp.Region.ap_northeast_1", "Region", "aws").is_err());
        // Wrong type name
        assert!(validate_enum_namespace("aws.Location.ap_northeast_1", "Region", "aws").is_err());
        // 3-part is invalid for 2-segment namespace (e.g., "aws.s3")
        assert!(
            validate_enum_namespace("vpc.InstanceTenancy.default", "InstanceTenancy", "aws.vpc")
                .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_4_part_valid() {
        assert!(
            validate_enum_namespace(
                "aws.s3.VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3"
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "aws.vpc.InstanceTenancy.default",
                "InstanceTenancy",
                "aws.vpc"
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "awscc.ec2_vpc.InstanceTenancy.default",
                "InstanceTenancy",
                "awscc.ec2_vpc"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_4_part_invalid() {
        // Wrong provider
        assert!(
            validate_enum_namespace(
                "awscc.s3.VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3"
            )
            .is_err()
        );
        // Wrong resource
        assert!(
            validate_enum_namespace(
                "aws.s.VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3"
            )
            .is_err()
        );
        // Wrong type name
        assert!(
            validate_enum_namespace("aws.s3.Versioning.Enabled", "VersioningStatus", "aws.s3")
                .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_wrong_part_count() {
        // Too many parts for 1-segment namespace
        assert!(validate_enum_namespace("foo.bar.baz.ap_northeast_1", "Region", "aws").is_err());
        // 5-part is invalid for 2-segment namespace
        assert!(validate_enum_namespace("a.b.c.d.e", "VersioningStatus", "aws.s3").is_err());
    }
}
