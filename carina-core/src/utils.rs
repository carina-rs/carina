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
}
