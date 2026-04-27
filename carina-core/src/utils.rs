//! Shared utility functions for value normalization and conversion

use crate::resource::Value;
use crate::schema::NamespacedEnumParts;

/// A namespaced DSL enum identifier, parsed once and reused.
///
/// The DSL accepts enum values in four shapes:
///
/// - `value` (no dots, e.g. `Enabled`, `ipsec.1` when treated atomically)
/// - `TypeName.value` (2-part, e.g. `Region.ap_northeast_1`)
/// - `provider.TypeName.value` (3-part, e.g. `aws.Region.ap_northeast_1`)
/// - `provider.<segments…>.TypeName.value` (4+ part, e.g.
///   `aws.s3.VersioningStatus.Enabled`,
///   `awscc.ec2.Vpc.InstanceTenancy.default`,
///   `awscc.ec2.vpn_gateway.Type.ipsec.1` — the trailing value may itself
///   contain dots).
///
/// `NamespacedId::parse` is the single source of truth for these shapes.
/// Adding a new shape (or relaxing a segment-character rule) means editing
/// this one parser instead of every utility that consumes identifiers.
#[derive(Debug, PartialEq, Eq)]
pub enum NamespacedId<'a> {
    /// `TypeName.value`
    TypeQualified { type_name: &'a str, value: &'a str },
    /// `provider.TypeName.value`
    ProviderQualified {
        provider: &'a str,
        type_name: &'a str,
        value: &'a str,
    },
    /// `provider.<segments…>.TypeName.value`
    ///
    /// `segments_str` is the dot-joined slice between `provider` and
    /// `type_name` (e.g. `s3`, or `ec2.Vpc`). Stored as one borrowed slice
    /// so `parse` does not have to allocate a `Vec` for the segment list.
    /// `value` is the rest of the string after `TypeName.` and may itself
    /// contain dots.
    FullyQualified {
        provider: &'a str,
        segments_str: &'a str,
        type_name: &'a str,
        value: &'a str,
    },
}

impl<'a> NamespacedId<'a> {
    /// Parse a namespaced identifier. Returns `None` for inputs that don't
    /// match any DSL enum shape (no dots, lowercase TypeName position,
    /// invalid segment characters, etc.).
    pub fn parse(s: &'a str) -> Option<Self> {
        let parts: Vec<&'a str> = s.split('.').collect();
        match parts.len() {
            0 | 1 => None,
            2 => {
                let (type_name, value) = (parts[0], parts[1]);
                if !is_type_name_segment(type_name) {
                    return None;
                }
                Some(Self::TypeQualified { type_name, value })
            }
            3 => {
                let (provider, type_name, value) = (parts[0], parts[1], parts[2]);
                if !is_provider_segment(provider) || !is_type_name_segment(type_name) {
                    return None;
                }
                Some(Self::ProviderQualified {
                    provider,
                    type_name,
                    value,
                })
            }
            4 => {
                let (provider, seg, type_name, value) = (parts[0], parts[1], parts[2], parts[3]);
                if !is_provider_segment(provider)
                    || !is_intermediate_segment(seg)
                    || !is_type_name_segment(type_name)
                {
                    return None;
                }
                Some(Self::FullyQualified {
                    provider,
                    segments_str: seg,
                    type_name,
                    value,
                })
            }
            // 5+ parts: provider.<service>.<resource>.TypeName.value, where
            // value may itself contain dots (e.g. `ipsec.1`). TypeName is
            // pinned at index 3 so that PascalCase resource segments
            // (`Vpc`, `Volume`) parse correctly and dotted values flow into
            // the trailing slice instead of being mistaken for TypeNames.
            _ => {
                let (provider, service, resource, type_name) =
                    (parts[0], parts[1], parts[2], parts[3]);
                if !is_provider_segment(provider)
                    || !is_service_segment(service)
                    || !is_intermediate_segment(resource)
                    || !is_type_name_segment(type_name)
                {
                    return None;
                }
                // Slice both `segments_str` and `value` directly out of `s`
                // so the parse keeps a single allocation (just `parts`).
                let segments_start = provider.len() + 1;
                let segments_end = segments_start + service.len() + 1 + resource.len();
                let value_start = segments_end + 1 + type_name.len() + 1;
                Some(Self::FullyQualified {
                    provider,
                    segments_str: &s[segments_start..segments_end],
                    type_name,
                    value: &s[value_start..],
                })
            }
        }
    }

    /// The trailing enum value, regardless of shape.
    pub fn value(&self) -> &'a str {
        match self {
            Self::TypeQualified { value, .. }
            | Self::ProviderQualified { value, .. }
            | Self::FullyQualified { value, .. } => value,
        }
    }

    /// The `TypeName` segment.
    pub fn type_name(&self) -> &'a str {
        match self {
            Self::TypeQualified { type_name, .. }
            | Self::ProviderQualified { type_name, .. }
            | Self::FullyQualified { type_name, .. } => type_name,
        }
    }

    /// True iff the identifier matches the expected `<namespace>.<TypeName>`
    /// prefix exactly. `expected_ns` is the dot-joined namespace (e.g. `aws`
    /// for a 3-part input, `aws.s3.Bucket` for a 5-part input); the part
    /// count must be `expected_ns.split('.').count() + 2`. 2-part
    /// `TypeName.value` inputs match when `type_name == expected_type`,
    /// regardless of the namespace.
    pub fn matches_namespace(&self, expected_ns: &str, expected_type: &str) -> bool {
        match self {
            Self::TypeQualified { type_name, .. } => *type_name == expected_type,
            Self::ProviderQualified {
                provider,
                type_name,
                ..
            } => {
                // 3-part inputs only match a 1-segment namespace; reject
                // multi-segment expectations to keep this method honest if
                // ever called outside `validate_enum_namespace`.
                *type_name == expected_type
                    && !expected_ns.contains('.')
                    && expected_ns == *provider
            }
            Self::FullyQualified {
                provider,
                segments_str,
                type_name,
                ..
            } => {
                if *type_name != expected_type {
                    return false;
                }
                let mut iter = expected_ns.split('.');
                let Some(expected_provider) = iter.next() else {
                    return false;
                };
                if expected_provider != *provider {
                    return false;
                }
                // Compare the remainder of `expected_ns` against
                // `segments_str` as raw string slices — no per-call Vec.
                iter.eq(segments_str.split('.'))
            }
        }
    }
}

/// A `provider` segment — lowercase ASCII only.
fn is_provider_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase())
}

/// A `service` segment (5+ part shape, sits at index 1) — lowercase ASCII
/// or digits; mirrors the original `is_dsl_enum_format` rule.
fn is_service_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// A segment that sits between `provider` and `TypeName` — accepts the
/// snake_case form (`s3`, `vpn_gateway`, `ipam_pool`) and the PascalCase
/// resource form (`Vpc`, `HostedZone`).
fn is_intermediate_segment(s: &str) -> bool {
    let snake = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let pascal = s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric());
    snake || pascal
}

/// A `TypeName` segment — first char uppercase ASCII.
fn is_type_name_segment(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

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
    // Whole input may itself be a raw enum value containing a dot
    // (e.g. `ipsec.1` with no namespace prefix).
    if valid_values.iter().any(|v| v.eq_ignore_ascii_case(s)) {
        return s;
    }
    // Walk uppercase-led segments from earliest to latest and check the
    // tail against valid_values. The earliest match wins so dotted values
    // like `ipsec.1` are recovered intact. Skip the very last segment —
    // there must be at least one segment after the candidate TypeName.
    let parts: Vec<&str> = s.split('.').collect();
    let last_idx = parts.len() - 1;
    let mut value_start = 0usize;
    for part in parts.iter().take(last_idx) {
        value_start += part.len() + 1;
        if !is_type_name_segment(part) {
            continue;
        }
        let candidate = &s[value_start..];
        if valid_values
            .iter()
            .any(|v| v.eq_ignore_ascii_case(candidate))
        {
            return candidate;
        }
    }
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
    NamespacedId::parse(value).map_or(value, |id| id.value())
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
    NamespacedId::parse(s).is_some()
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

    // Reject dotted values that exceed the strict part count for the
    // expected namespace shape — callers must strip those before validating.
    let actual_parts = s.split('.').count();
    let expected_full_len = namespace.split('.').count() + 2;
    let is_two_part = actual_parts == 2;
    let is_full_form = actual_parts == expected_full_len;
    if !is_two_part && !is_full_form {
        return Err(format!(
            "expected format: value, {}.value, or {}.{}.value",
            type_name, namespace, type_name
        ));
    }

    if let Some(id) = NamespacedId::parse(s)
        && id.matches_namespace(namespace, type_name)
    {
        return Ok(());
    }
    // Mirror the original error-message shape: 2-part inputs get the
    // "or full form" hint, full-form inputs get only the full form.
    if is_two_part {
        Err(format!(
            "expected format {}.value or {}.{}.value",
            type_name, namespace, type_name
        ))
    } else {
        Err(format!("expected format {}.{}.value", namespace, type_name))
    }
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

    // Table-driven coverage for #2221. The per-case tests above each pin one
    // shape; these tables enumerate the cross-cutting dimensions that no
    // single per-case test exercises together: dotted values at sub-5-part
    // shapes, digits-only values, hyphen-bearing segments, and digit-bearing
    // resource segments beyond the `s3`/`ec2` already covered. When a new
    // namespaced-identifier shape lands (e.g. #2213), append rows here.

    #[test]
    fn extract_enum_value_with_values_table() {
        let cases: &[(&str, &[&str], &str)] = &[
            // Dotted value at sub-5-part shapes (5-part is already pinned by
            // test_extract_enum_value_with_values_dotted).
            ("ipsec.1", &["ipsec.1"], "ipsec.1"),
            ("Type.ipsec.1", &["ipsec.1"], "ipsec.1"),
            ("aws.Type.ipsec.1", &["ipsec.1"], "ipsec.1"),
            ("aws.ec2.Type.ipsec.1", &["ipsec.1"], "ipsec.1"),
            ("aws.ec2.Volume.Iops.100", &["100", "200"], "100"),
            ("Iops.1", &["1"], "1"),
            (
                "aws.Region.ap_northeast_1",
                &["ap_northeast_1"],
                "ap_northeast_1",
            ),
            ("aws.ec2.Volume.Type.gp2", &["gp2", "gp3"], "gp2"),
            ("aws.ec2.Volume.Type.gp_2", &["gp_2"], "gp_2"),
            ("aws.route53.RecordType.A", &["A", "AAAA", "CNAME"], "A"),
            (
                "awscc.route53.HostedZone.RecordType.AAAA",
                &["A", "AAAA"],
                "AAAA",
            ),
            ("aws.ec2.Volume.Type.io1", &["io1", "io2"], "io1"),
            // TypeName whose lowercased form is also a valid value: the
            // function must skip the TypeName segment and return the trailing
            // value, not the type name itself.
            ("Default.default", &["default", "other"], "default"),
        ];
        for (input, valid, expected) in cases {
            assert_eq!(
                extract_enum_value_with_values(input, valid),
                *expected,
                "input={input:?} valid={valid:?}",
            );
        }
    }

    #[test]
    fn is_dsl_enum_format_table() {
        let cases: &[(&str, bool)] = &[
            ("Type.gp2", true),
            ("Iops.100", true),
            ("region.ap_northeast_1", false),
            ("AWS.Region.ap_northeast_1", false),
            ("aws.route53.RecordType.A", true),
            ("aws.s-3.VersioningStatus.Enabled", false),
            ("awscc.route53.HostedZone.RecordType.A", true),
            ("awscc.ec2.ipam_pool.AddressFamily.IPv4", true),
            ("awscc.ec2.vpn_gateway.Type.ipsec.1", true),
            ("some.random.string", false),
            ("a.b.c.d.e.f", false),
            ("aws.s3.versioningstatus.Enabled", false),
        ];
        for (input, expected) in cases {
            assert_eq!(is_dsl_enum_format(input), *expected, "input={input:?}");
        }
    }

    #[test]
    fn convert_enum_value_table() {
        let cases: &[(&str, &str)] = &[
            ("Type.gp2", "gp2"),
            ("Iops.100", "100"),
            ("aws.Region.ap_northeast_1", "ap_northeast_1"),
            ("aws.route53.RecordType.AAAA", "AAAA"),
            ("aws.ec2.Volume.Iops.100", "100"),
            ("awscc.ec2.vpn_gateway.Type.ipsec.1", "ipsec.1"),
            ("awscc.ec2.Vpc.InstanceTenancy.default", "default"),
            ("TargetType.AWS_ACCOUNT", "AWS_ACCOUNT"),
            ("awscc.sso.Assignment.TargetType.AWS_ACCOUNT", "AWS_ACCOUNT"),
            ("eu-west-1", "eu-west-1"),
            ("region.us_east_1", "region.us_east_1"),
            ("Enabled", "Enabled"),
        ];
        for (input, expected) in cases {
            assert_eq!(convert_enum_value(input), *expected, "input={input:?}");
        }
    }

    #[test]
    fn validate_enum_namespace_table() {
        let cases: &[(&str, &str, &str, bool)] = &[
            ("Enabled", "VersioningStatus", "aws.s3.Bucket", true),
            ("100", "Iops", "aws.ec2.Volume", true),
            ("Iops.100", "Iops", "aws.ec2.Volume", true),
            ("Iops.100", "Tenancy", "aws.ec2.Volume", false),
            ("aws.Region.ap_northeast_1", "Region", "aws", true),
            ("gcp.Region.ap_northeast_1", "Region", "aws", false),
            (
                "aws.s3.VersioningStatus.Enabled",
                "VersioningStatus",
                "aws.s3",
                true,
            ),
            (
                "awscc.ec2.Vpc.InstanceTenancy.default",
                "InstanceTenancy",
                "awscc.ec2.Vpc",
                true,
            ),
            // Dotted values like `ipsec.1` blow the strict part-count rule —
            // callers must strip them before validating.
            (
                "awscc.ec2.vpn_gateway.Type.ipsec.1",
                "Type",
                "awscc.ec2.vpn_gateway",
                false,
            ),
            ("aws.Region.aws.Region.us_west_2", "Region", "aws", false),
            ("foo.bar.baz.ap_northeast_1", "Region", "aws", false),
            (
                "awscc.route53.HostedZone.RecordType.A",
                "RecordType",
                "awscc.route53.HostedZone",
                true,
            ),
        ];
        for (input, type_name, namespace, ok) in cases {
            let result = validate_enum_namespace(input, type_name, namespace);
            assert_eq!(
                result.is_ok(),
                *ok,
                "input={input:?} type_name={type_name:?} ns={namespace:?} got={result:?}",
            );
        }
    }
}
