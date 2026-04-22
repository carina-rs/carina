//! Shared utility functions for value normalization and conversion

use crate::resource::Value;
use crate::schema::NamespacedEnumParts;

/// Extract the last dot-separated part from a namespaced identifier.
/// Returns the original string if no dots are present.
///
/// # Examples
///
/// ```
/// use carina_core::utils::extract_enum_value;
///
/// assert_eq!(extract_enum_value("aws.Region.ap_northeast_1"), "ap_northeast_1");
/// assert_eq!(extract_enum_value("aws.s3.Bucket.VersioningStatus.Enabled"), "Enabled");
/// assert_eq!(extract_enum_value("Enabled"), "Enabled");
/// ```
pub fn extract_enum_value(s: &str) -> &str {
    if s.contains('.') {
        s.split('.').next_back().unwrap_or(s)
    } else {
        s
    }
}

/// Extract the enum value from a namespaced identifier using known valid values
/// for disambiguation.
///
/// When enum values themselves contain dots (e.g., `"ipsec.1"`), the simple
/// last-dot-segment approach of [`extract_enum_value`] produces wrong results.
/// This function checks each possible split point against the known valid values
/// to find the correct enum value.
///
/// Falls back to [`extract_enum_value`] if no valid values match.
///
/// # Examples
///
/// ```
/// use carina_core::utils::extract_enum_value_with_values;
///
/// let values = &["ipsec.1"];
/// assert_eq!(
///     extract_enum_value_with_values("awscc.ec2.vpn_gateway.Type.ipsec.1", values),
///     "ipsec.1"
/// );
///
/// let values = &["default", "dedicated", "host"];
/// assert_eq!(
///     extract_enum_value_with_values("awscc.ec2.Vpc.InstanceTenancy.default", values),
///     "default"
/// );
/// ```
pub fn extract_enum_value_with_values<'a>(s: &'a str, valid_values: &[&str]) -> &'a str {
    if !s.contains('.') {
        return s;
    }
    // First, check if the entire string directly matches a valid value (e.g., "ipsec.1").
    // This handles cases where the value contains dots but is not a namespaced identifier.
    if valid_values.iter().any(|v| v.eq_ignore_ascii_case(s)) {
        return s;
    }
    // Try each possible split point after a dot, from earliest to latest.
    // The value portion is everything after the namespace prefix and type name.
    // For "awscc.ec2.vpn_gateway.Type.ipsec.1", we try:
    //   "ec2.vpn_gateway.Type.ipsec.1", "vpn_gateway.Type.ipsec.1", "Type.ipsec.1",
    //   "ipsec.1", "1"
    // We want the earliest match against valid_values, but we need at least
    // a namespace prefix, so we check suffixes that could be the value part.
    // The value is after the TypeName part. Since TypeName starts with uppercase,
    // find the TypeName position and take everything after it.
    let parts: Vec<&str> = s.split('.').collect();
    // Find the TypeName part (starts with uppercase) to determine where the value begins
    for (i, part) in parts.iter().enumerate() {
        if part.chars().next().is_some_and(|c| c.is_uppercase()) && i + 1 < parts.len() {
            // The value is everything after this TypeName part
            let value_start = parts[..=i].iter().map(|p| p.len() + 1).sum::<usize>();
            let candidate = &s[value_start..];
            // Check if this candidate matches a valid value (case-insensitive)
            if valid_values
                .iter()
                .any(|v| v.eq_ignore_ascii_case(candidate))
            {
                return candidate;
            }
        }
    }
    // Fallback to simple last-segment extraction
    extract_enum_value(s)
}

/// Strip the namespace prefix from a DSL enum identifier and return the raw value.
///
/// Handles the following patterns:
/// - 2-part: `TypeName.value_name` -> `value_name`
/// - 3-part: `provider.TypeName.value_name` -> `value_name`
/// - 4-part: `provider.resource.TypeName.value_name` -> `value_name`
/// - 5-part: `provider.service.resource.TypeName.value_name` -> `value_name`
///
/// The first component of TypeName must be uppercase.
/// The extracted value is returned as-is without any transformation.
/// Returns the original value unchanged if it doesn't match any pattern.
///
/// # Examples
///
/// ```
/// use carina_core::utils::convert_enum_value;
///
/// assert_eq!(convert_enum_value("aws.Region.ap_northeast_1"), "ap_northeast_1");
/// assert_eq!(convert_enum_value("Region.ap_northeast_1"), "ap_northeast_1");
/// assert_eq!(convert_enum_value("awscc.ec2.ipam.Tier.advanced"), "advanced");
/// assert_eq!(convert_enum_value("eu-west-1"), "eu-west-1");
/// ```
pub fn convert_enum_value(value: &str) -> &str {
    let parts: Vec<&str> = value.split('.').collect();
    match parts.len() {
        2 => {
            // TypeName.value pattern
            if parts[0].chars().next().is_some_and(|c| c.is_uppercase()) {
                parts[1]
            } else {
                value
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
                value
            }
        }
        // 4-part: provider.resource.TypeName.value
        // e.g., "aws.s3.VersioningStatus.Enabled" -> "Enabled"
        4 => {
            let provider = parts[0];
            let type_name = parts[2];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                parts[3]
            } else {
                value
            }
        }
        // 5+ part: provider.service.resource.TypeName.value (value may contain dots)
        // e.g., "awscc.ec2.Vpc.InstanceTenancy.default" -> "default"
        // e.g., "awscc.ec2.vpn_gateway.Type.ipsec.1" -> "ipsec.1"
        n if n >= 5 => {
            let provider = parts[0];
            let type_name = parts[3];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                // Rejoin all parts after TypeName (index 3) to handle values with dots
                let value_start = parts[..4].iter().map(|p| p.len() + 1).sum::<usize>();
                &value[value_start..]
            } else {
                value
            }
        }
        _ => value,
    }
}

/// Check if a string is in DSL enum format (a namespaced identifier).
///
/// Recognizes the following patterns:
/// - `TypeName.value` (2-part, e.g., `Region.ap_northeast_1`)
/// - `provider.TypeName.value` (3-part, e.g., `aws.Region.ap_northeast_1`)
/// - `provider.resource.TypeName.value` (4-part, e.g., `aws.s3.VersioningStatus.Enabled`)
/// - `provider.service.resource.TypeName.value` (5-part, e.g., `awscc.ec2.Vpc.InstanceTenancy.default`)
///
/// # Examples
///
/// ```
/// use carina_core::utils::is_dsl_enum_format;
///
/// assert!(is_dsl_enum_format("Region.ap_northeast_1"));
/// assert!(is_dsl_enum_format("aws.Region.ap_northeast_1"));
/// assert!(is_dsl_enum_format("aws.s3.VersioningStatus.Enabled"));
/// assert!(is_dsl_enum_format("awscc.ec2.Vpc.InstanceTenancy.default"));
/// assert!(!is_dsl_enum_format("my-bucket"));
/// assert!(!is_dsl_enum_format("some.random.string"));
/// ```
pub fn is_dsl_enum_format(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();

    match parts.len() {
        // TypeName.value
        2 => parts[0].chars().next().is_some_and(|c| c.is_uppercase()),
        // provider.TypeName.value
        3 => {
            let provider = parts[0];
            let type_name = parts[1];
            // provider should be lowercase, TypeName should start with uppercase
            provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
        }
        // provider.resource.TypeName.value (e.g., aws.s3.VersioningStatus.Enabled)
        4 => {
            let provider = parts[0];
            let resource = parts[1];
            let type_name = parts[2];
            // provider and resource should be lowercase/digits, TypeName should start with uppercase
            provider.chars().all(|c| c.is_lowercase())
                && resource
                    .chars()
                    .all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_')
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
        }
        // provider.service.resource.TypeName.value (e.g., awscc.ec2.Vpc.InstanceTenancy.default)
        // Also handles 6+ parts where the enum value itself contains dots
        // (e.g., awscc.ec2.vpn_gateway.Type.ipsec.1)
        // The `resource` segment accepts either the legacy snake_case form or
        // the naming-conventions PascalCase form (e.g., `Vpc`), since resource
        // kinds have been PascalCased.
        n if n >= 5 => {
            let provider = parts[0];
            let service = parts[1];
            let resource = parts[2];
            let type_name = parts[3];
            let resource_is_snake = resource
                .chars()
                .all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_');
            let resource_is_pascal = resource.chars().next().is_some_and(|c| c.is_uppercase())
                && resource.chars().all(|c| c.is_ascii_alphanumeric());
            provider.chars().all(|c| c.is_lowercase())
                && service
                    .chars()
                    .all(|c| c.is_lowercase() || c.is_ascii_digit())
                && (resource_is_snake || resource_is_pascal)
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
        }
        _ => false,
    }
}

/// Validate namespace format for an enum identifier.
///
/// Handles the following formats:
/// - No dots: passes through (not a namespaced identifier)
/// - `TypeName.value` (2-part): validates that the first part matches `type_name`
/// - Full namespaced form: exactly `namespace_parts + 2` parts, matching
///   `<namespace>.<TypeName>.<value>` with no extra segments
///
/// The expected full form length is determined by the namespace:
/// - `"aws"` (1 segment) → 3 parts: `aws.Region.value`
/// - `"aws.s3"` (2 segments) → 4 parts: `aws.s3.VersioningStatus.value`
///
/// Callers that need to accept enum values containing dots (e.g., `"ipsec.1"`)
/// must handle that case themselves before calling this function.
///
/// # Arguments
/// * `s` - The input string to validate
/// * `type_name` - Expected type name (e.g., `"Region"`, `"InstanceTenancy"`)
/// * `namespace` - Expected namespace prefix (e.g., `"aws"`, `"aws.s3.Bucket"`, `"awscc.ec2.Vpc"`)
///
/// # Returns
/// * `Ok(())` if namespace is valid or string has no dots
/// * `Err(String)` with bare reason string (without the input value) if namespace is invalid
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
/// assert!(validate_enum_namespace("aws.s3.Bucket.VersioningStatus.Enabled", "VersioningStatus", "aws.s3.Bucket").is_ok());
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
                    "expected format {}.value or {}.{}.value",
                    type_name, namespace, type_name
                ));
            }
        }
        // Full namespaced form: namespace.TypeName.value — strictly
        // `ns_parts.len() + 2` parts. Any extra parts are malformed.
        n if n == expected_full_len => {
            for (i, &expected) in ns_parts.iter().enumerate() {
                if parts[i] != expected {
                    return Err(format!("expected format {}.{}.value", namespace, type_name));
                }
            }
            if parts[ns_parts.len()] != type_name {
                return Err(format!("expected format {}.{}.value", namespace, type_name));
            }
        }
        _ => {
            return Err(format!(
                "expected format: value, {}.value, or {}.{}.value",
                type_name, namespace, type_name
            ));
        }
    }

    Ok(())
}

/// Resolve a single string value to its fully-qualified namespaced DSL format.
///
/// Given a value and the enum parts (type_name, namespace, optional to_dsl converter),
/// resolves:
/// - Bare identifiers: `"Enabled"` -> `"aws.s3.Bucket.VersioningStatus.Enabled"`
/// - TypeName.value shorthand: `"VersioningStatus.Enabled"` -> `"aws.s3.Bucket.VersioningStatus.Enabled"`
/// - Already-qualified values pass through unchanged (via the `_` arm)
///
/// Returns `None` if the value doesn't need resolution (non-String or already qualified).
pub fn resolve_enum_value(value: &Value, parts: &NamespacedEnumParts<'_>) -> Option<Value> {
    let (type_name, ns, to_dsl) = parts;
    match value {
        Value::String(s) if !s.contains('.') => {
            // bare identifier: "Enabled" -> ns.TypeName.Enabled
            let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
            Some(Value::String(format!("{}.{}.{}", ns, type_name, dsl_val)))
        }
        Value::String(s)
            if s.split_once('.')
                .is_some_and(|(ident, member)| ident == *type_name && !member.contains('.')) =>
        {
            // TypeName.value: "VersioningStatus.Enabled" -> ns.TypeName.Enabled
            let member = s.split_once('.').unwrap().1;
            let dsl_val = to_dsl.map_or_else(|| member.to_string(), |f| f(member));
            Some(Value::String(format!("{}.{}.{}", ns, type_name, dsl_val)))
        }
        _ => None,
    }
}

/// Normalize a single state enum value to its fully-qualified namespaced DSL format.
///
/// Unlike `resolve_enum_value`, this also handles values that contain dots but are
/// raw enum values (e.g., `"ipsec.1"`) rather than already-namespaced identifiers.
/// Uses `string_enum_parts` from the attribute type to distinguish between the two cases.
///
/// Returns `None` if the value doesn't need normalization.
pub fn normalize_state_enum_value(
    value: &Value,
    parts: &NamespacedEnumParts<'_>,
    string_enum_check: Option<&dyn Fn(&str) -> bool>,
) -> Option<Value> {
    let (type_name, ns, to_dsl) = parts;
    if let Value::String(s) = value {
        // Skip values already in namespaced DSL format.
        // A value that contains '.' but is not already namespaced is a raw enum value
        // like "ipsec.1" -- check if it matches a known valid enum value.
        let already_namespaced =
            s.contains('.') && !string_enum_check.is_some_and(|check| check(s));
        if !already_namespaced {
            let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
            return Some(Value::String(format!("{}.{}.{}", ns, type_name, dsl_val)));
        }
    }
    None
}

/// Convert a region value from DSL format to AWS SDK format.
///
/// Handles the following patterns:
/// - `aws.Region.ap_northeast_1` -> `ap-northeast-1`
/// - `awscc.Region.ap_northeast_1` -> `ap-northeast-1`
/// - `ap-northeast-1` -> `ap-northeast-1` (passthrough)
///
/// # Examples
///
/// ```
/// use carina_core::utils::convert_region_value;
///
/// assert_eq!(convert_region_value("aws.Region.ap_northeast_1"), "ap-northeast-1");
/// assert_eq!(convert_region_value("awscc.Region.us_west_2"), "us-west-2");
/// assert_eq!(convert_region_value("eu-west-1"), "eu-west-1");
/// ```
pub fn convert_region_value(value: &str) -> String {
    // Match any `<provider>.Region.<region_name>` pattern (e.g., "aws.Region.ap_northeast_1")
    if let Some(pos) = value.find(".Region.") {
        let rest = &value[pos + ".Region.".len()..];
        // Verify the prefix is a simple provider name (no dots except the one before Region)
        if !value[..pos].contains('.') {
            return rest.replace('_', "-");
        }
    }
    value.to_string()
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
            extract_enum_value("aws.s3.Bucket.VersioningStatus.Enabled"),
            "Enabled"
        );
        assert_eq!(
            extract_enum_value("awscc.ec2.Vpc.InstanceTenancy.default"),
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
            "ap_northeast_1"
        );
    }

    #[test]
    fn test_convert_enum_value_3_part() {
        assert_eq!(
            convert_enum_value("aws.Region.ap_northeast_1"),
            "ap_northeast_1"
        );
        assert_eq!(convert_enum_value("aws.Region.us_east_1"), "us_east_1");
        assert_eq!(
            convert_enum_value("aws.AvailabilityZone.ap_northeast_1a"),
            "ap_northeast_1a"
        );
        assert_eq!(
            convert_enum_value("aws.AvailabilityZone.us_east_1b"),
            "us_east_1b"
        );
    }

    #[test]
    fn test_convert_enum_value_4_part() {
        assert_eq!(
            convert_enum_value("aws.s3.VersioningStatus.Enabled"),
            "Enabled"
        );
        assert_eq!(convert_enum_value("aws.ec2.IpProtocol.tcp"), "tcp");
    }

    #[test]
    fn test_convert_enum_value_5_part() {
        assert_eq!(
            convert_enum_value("awscc.ec2.ipam.Tier.advanced"),
            "advanced"
        );
        assert_eq!(
            convert_enum_value("awscc.ec2.ipam_pool.AddressFamily.IPv4"),
            "IPv4"
        );
        assert_eq!(
            convert_enum_value("awscc.ec2.Vpc.InstanceTenancy.default"),
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
        assert!(validate_enum_namespace("Enabled", "VersioningStatus", "aws.s3.Bucket").is_ok());
        assert!(validate_enum_namespace("ap-northeast-1", "Region", "aws").is_ok());
        assert!(validate_enum_namespace("default", "InstanceTenancy", "awscc.ec2.Vpc").is_ok());
    }

    #[test]
    fn test_validate_namespace_2_part_valid() {
        assert!(validate_enum_namespace("Region.ap_northeast_1", "Region", "aws").is_ok());
        assert!(
            validate_enum_namespace(
                "VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3.Bucket"
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "InstanceTenancy.default",
                "InstanceTenancy",
                "awscc.ec2.Vpc"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_2_part_invalid() {
        assert!(validate_enum_namespace("Location.ap_northeast_1", "Region", "aws").is_err());
        assert!(
            validate_enum_namespace("Versioning.Enabled", "VersioningStatus", "aws.s3.Bucket")
                .is_err()
        );
        assert!(
            validate_enum_namespace("Tenancy.default", "InstanceTenancy", "awscc.ec2.Vpc").is_err()
        );
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
    }

    #[test]
    fn test_validate_namespace_4_part_valid() {
        // 4-part is valid for 2-segment namespace (e.g., "aws.s3")
        assert!(
            validate_enum_namespace(
                "aws.s3.VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_4_part_invalid() {
        // Wrong provider
        assert!(
            validate_enum_namespace(
                "gcp.s3.VersioningStatus.Enabled",
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
    fn test_validate_namespace_5_part_valid() {
        assert!(
            validate_enum_namespace(
                "aws.s3.Bucket.VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3.Bucket"
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "awscc.ec2.Vpc.InstanceTenancy.default",
                "InstanceTenancy",
                "awscc.ec2.Vpc"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_5_part_invalid() {
        // Wrong provider
        assert!(
            validate_enum_namespace(
                "gcp.ec2.vpc.InstanceTenancy.default",
                "InstanceTenancy",
                "awscc.ec2.Vpc"
            )
            .is_err()
        );
        // Wrong type name
        assert!(
            validate_enum_namespace(
                "awscc.ec2.Vpc.Tenancy.default",
                "InstanceTenancy",
                "awscc.ec2.Vpc"
            )
            .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_wrong_part_count() {
        // Too many parts for 1-segment namespace
        assert!(validate_enum_namespace("foo.bar.baz.ap_northeast_1", "Region", "aws").is_err());
        // 6-part is invalid for 3-segment namespace
        assert!(
            validate_enum_namespace("a.b.c.d.e.f", "VersioningStatus", "aws.s3.Bucket").is_err()
        );
    }

    // is_dsl_enum_format tests

    #[test]
    fn test_is_dsl_enum_format_2_part() {
        assert!(is_dsl_enum_format("Region.ap_northeast_1"));
        assert!(is_dsl_enum_format("VersioningStatus.Enabled"));
        // lowercase first part → not a TypeName
        assert!(!is_dsl_enum_format("region.ap_northeast_1"));
    }

    #[test]
    fn test_is_dsl_enum_format_3_part() {
        assert!(is_dsl_enum_format("aws.Region.ap_northeast_1"));
        assert!(is_dsl_enum_format("gcp.Region.us_central1"));
        // uppercase provider → not valid
        assert!(!is_dsl_enum_format("AWS.Region.ap_northeast_1"));
        // lowercase TypeName → not valid
        assert!(!is_dsl_enum_format("aws.region.ap_northeast_1"));
    }

    #[test]
    fn test_is_dsl_enum_format_4_part() {
        assert!(is_dsl_enum_format("aws.s3.VersioningStatus.Enabled"));
        assert!(is_dsl_enum_format("aws.ec2.IpProtocol.tcp"));
        // resource with digits
        assert!(is_dsl_enum_format("aws.s3.VersioningStatus.Suspended"));
    }

    #[test]
    fn test_is_dsl_enum_format_5_part() {
        assert!(is_dsl_enum_format("awscc.ec2.Vpc.InstanceTenancy.default"));
        assert!(is_dsl_enum_format("awscc.ec2.ipam_pool.AddressFamily.IPv4"));
    }

    #[test]
    fn test_is_dsl_enum_format_6_part_dotted_value() {
        // 6-part: provider.service.resource.TypeName.value.with.dots
        assert!(is_dsl_enum_format("awscc.ec2.vpn_gateway.Type.ipsec.1"));
    }

    #[test]
    fn test_is_dsl_enum_format_non_matching() {
        assert!(!is_dsl_enum_format("my-bucket"));
        assert!(!is_dsl_enum_format("ap-northeast-1"));
        assert!(!is_dsl_enum_format("some.random.string"));
        assert!(!is_dsl_enum_format("a.b.c.d.e.f"));
    }

    // extract_enum_value_with_values tests

    #[test]
    fn test_extract_enum_value_with_values_dotted() {
        let values = &["ipsec.1"];
        assert_eq!(
            extract_enum_value_with_values("awscc.ec2.vpn_gateway.Type.ipsec.1", values),
            "ipsec.1"
        );
    }

    #[test]
    fn test_extract_enum_value_with_values_simple() {
        let values = &["default", "dedicated", "host"];
        assert_eq!(
            extract_enum_value_with_values("awscc.ec2.Vpc.InstanceTenancy.default", values),
            "default"
        );
    }

    #[test]
    fn test_extract_enum_value_with_values_no_dots() {
        let values = &["Enabled", "Suspended"];
        assert_eq!(extract_enum_value_with_values("Enabled", values), "Enabled");
    }

    #[test]
    fn test_extract_enum_value_with_values_fallback() {
        // When no valid value matches, falls back to last segment
        let values = &["foo", "bar"];
        assert_eq!(
            extract_enum_value_with_values("awscc.ec2.Vpc.InstanceTenancy.default", values),
            "default"
        );
    }

    #[test]
    fn test_validate_namespace_strict_rejects_extra_parts() {
        // validate_enum_namespace is strict: it rejects any part count other
        // than the exact expected_full_len. Callers handling dotted values
        // (like "ipsec.1") must do so separately before calling this.
        assert!(
            validate_enum_namespace(
                "awscc.ec2.vpn_gateway.Type.ipsec.1",
                "Type",
                "awscc.ec2.vpn_gateway"
            )
            .is_err()
        );
        // Double-namespace patterns are also rejected.
        assert!(
            validate_enum_namespace(
                "awscc.Region.awscc.Region.ap_northeast_1",
                "Region",
                "awscc"
            )
            .is_err()
        );
        assert!(
            validate_enum_namespace("aws.Region.aws.Region.us_west_2", "Region", "aws").is_err()
        );
    }

    // convert_enum_value with dotted values

    #[test]
    fn test_convert_enum_value_6_part_dotted_value() {
        assert_eq!(
            convert_enum_value("awscc.ec2.vpn_gateway.Type.ipsec.1"),
            "ipsec.1"
        );
    }

    // convert_enum_value: underscores in enum values must be preserved (#1675)

    #[test]
    fn test_convert_enum_value_preserves_underscores() {
        // Enum values like AWS_ACCOUNT must not have underscores converted to hyphens
        assert_eq!(
            convert_enum_value("awscc.sso.Assignment.TargetType.AWS_ACCOUNT"),
            "AWS_ACCOUNT"
        );
        assert_eq!(convert_enum_value("TargetType.AWS_ACCOUNT"), "AWS_ACCOUNT");
        assert_eq!(
            convert_enum_value("awscc.sso.Assignment.PrincipalType.GROUP"),
            "GROUP"
        );
    }

    // convert_region_value tests

    #[test]
    fn test_convert_region_value_aws_prefix() {
        assert_eq!(
            convert_region_value("aws.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_region_value("aws.Region.us_west_2"), "us-west-2");
    }

    #[test]
    fn test_convert_region_value_awscc_prefix() {
        assert_eq!(
            convert_region_value("awscc.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_region_value("awscc.Region.us_west_2"), "us-west-2");
    }

    #[test]
    fn test_convert_region_value_passthrough() {
        assert_eq!(convert_region_value("us-east-1"), "us-east-1");
        assert_eq!(convert_region_value("eu-west-1"), "eu-west-1");
    }
}
