//! Shared utility functions for value normalization and conversion

use crate::resource::{ConcreteValue, Value};
use crate::schema::{AttributeType, NamespacedEnumParts};

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

/// Expand a user-written enum value into its fully-qualified DSL form.
///
/// Accepts the three input shapes the DSL allows for `StringEnum` and
/// enum-like `Custom` attributes:
///
/// - bare member (`dedicated`) → `<namespace>.<name>.dedicated`
/// - `TypeName.member` shorthand (`InstanceTenancy.dedicated`) →
///   `<namespace>.<name>.dedicated`, only when `TypeName == name`
/// - any other input (already-qualified, foreign type name, missing
///   namespace, non-string) → returned unchanged
///
/// Used by both `AttributeType::resolve_value` and the LSP diagnostic
/// pipeline so the two paths cannot drift.
pub fn expand_enum_shorthand(value: &Value, name: &str, namespace: Option<&str>) -> Value {
    // Phase 4 of carina#2986: `EnumIdentifier` carries the same textual
    // payload as `String` (only the source-shape tag differs) and goes
    // through the same namespace expansion. The result is materialized as
    // `String` because every downstream consumer
    // (`validate_string_enum`, the LSP completion helpers, the
    // builtin-result coercion in `convert_enum_value`) reads through the
    // `Value::Concrete(ConcreteValue::String)` arm. The `EnumIdentifier`
    // distinction is a parser-level signal used for strict shape
    // enforcement at the validator entry, not a wire-level form.
    let text_form: Option<&str> = match value {
        Value::Concrete(ConcreteValue::String(s)) => Some(s.as_str()),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Some(s.as_str()),
        _ => None,
    };
    match text_form {
        Some(s) if !s.contains('.') => match namespace {
            Some(ns) => Value::Concrete(ConcreteValue::String(format!("{}.{}.{}", ns, name, s))),
            None => Value::Concrete(ConcreteValue::String(s.to_string())),
        },
        Some(s) => {
            if let Some(NamespacedId::TypeQualified {
                type_name: ident,
                value: member,
            }) = NamespacedId::parse(s)
                && let Some(ns) = namespace
                && ident == name
            {
                Value::Concrete(ConcreteValue::String(format!(
                    "{}.{}.{}",
                    ns, ident, member
                )))
            } else {
                Value::Concrete(ConcreteValue::String(s.to_string()))
            }
        }
        None => value.clone(),
    }
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
/// Given a value and the enum parts (type_name, namespace, DSL alias map),
/// resolves:
/// - Bare identifiers: `"Enabled"` -> `"aws.s3.Bucket.VersioningStatus.Enabled"`
/// - TypeName.value shorthand: `"VersioningStatus.Enabled"` -> `"aws.s3.Bucket.VersioningStatus.Enabled"`
/// - Already-qualified values pass through unchanged (via the `_` arm)
///
/// Returns `None` if the value doesn't need resolution (non-text or already qualified).
///
/// Phase 4 of carina#2986: bare DSL enum values arrive as
/// `ConcreteValue::EnumIdentifier`, not `ConcreteValue::String`, when
/// they come straight from the parser. Both carry the same textual
/// payload and resolve identically — only the source-shape tag differs —
/// so this matches the [`expand_enum_shorthand`] convention: extract the
/// text from either variant and materialize the resolved value as
/// `String` (every downstream consumer reads through the `String` arm;
/// the `EnumIdentifier` tag is a parser-level strict-shape signal, not a
/// wire form). Without the `EnumIdentifier` arm, bare struct-field enums
/// (e.g. `effect = allow`) were left unresolved and diverged from the
/// AWS-read side, which produces the fully-qualified form (aws#313).
pub fn resolve_enum_value(value: &Value, parts: &NamespacedEnumParts<'_>) -> Option<Value> {
    let (type_name, ns, dsl_map) = parts;
    let s = match value {
        Value::Concrete(ConcreteValue::String(s)) => s.as_str(),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => s.as_str(),
        _ => return None,
    };
    if !s.contains('.') {
        // bare identifier: "Enabled" -> ns.TypeName.Enabled
        let dsl_val = dsl_map.dsl_for(s);
        return Some(Value::Concrete(ConcreteValue::String(format!(
            "{}.{}.{}",
            ns, type_name, dsl_val
        ))));
    }
    if let Some((ident, member)) = s.split_once('.')
        && ident == *type_name
        && !member.contains('.')
    {
        // TypeName.value: "VersioningStatus.Enabled" -> ns.TypeName.Enabled
        let dsl_val = dsl_map.dsl_for(member);
        return Some(Value::Concrete(ConcreteValue::String(format!(
            "{}.{}.{}",
            ns, type_name, dsl_val
        ))));
    }
    None
}

/// Resolve every enum value reachable from `value` through `attr_type` to
/// its fully-qualified namespaced DSL format, descending into struct
/// fields, list elements, and map values.
///
/// At each position the function asks the schema what type of value
/// lives there. If the position is a `StringEnum` (or a `Custom` with a
/// namespaced enum), [`resolve_enum_value`] runs on it. If the position
/// is a `Struct`/`List`/`Map`, recursion continues into the children.
/// All other types pass through unchanged.
///
/// Returns `Some(new_value)` when at least one nested value was
/// rewritten, `None` when nothing changed (mirroring
/// [`resolve_enum_value`]'s contract so callers can keep a "rewrite
/// only on diff" pattern).
///
/// `Union` types are not recursed into — there is no way to tell which
/// arm a runtime value belongs to without re-running the validator, and
/// AWS schemas use `Union` only for shapes where each arm is itself a
/// scalar (so there are no nested enums to find anyway).
///
/// # Examples
///
/// ```
/// use carina_core::resource::{ConcreteValue, Value};
/// use carina_core::schema::{AttributeType, StructField};
/// use carina_core::utils::resolve_enum_value_recursive;
/// use indexmap::IndexMap;
///
/// let status_enum = AttributeType::StringEnum {
///     name: "VersioningStatus".to_string(),
///     values: vec!["Enabled".to_string(), "Suspended".to_string()],
///     namespace: Some("aws.s3.Bucket".to_string()),
///     dsl_aliases: vec![],
/// };
/// let config = AttributeType::Struct {
///     name: "VersioningConfiguration".to_string(),
///     fields: vec![StructField::new("status", status_enum)],
/// };
///
/// let mut inner = IndexMap::new();
/// inner.insert("status".to_string(),
///     Value::Concrete(ConcreteValue::String("Enabled".to_string())));
/// let input = Value::Concrete(ConcreteValue::Map(inner));
///
/// let resolved = resolve_enum_value_recursive(&input, &config).unwrap();
/// // Bare "Enabled" → fully-qualified DSL form.
/// match resolved {
///     Value::Concrete(ConcreteValue::Map(m)) => {
///         match m.get("status").unwrap() {
///             Value::Concrete(ConcreteValue::String(s)) => {
///                 assert_eq!(s, "aws.s3.Bucket.VersioningStatus.Enabled");
///             }
///             _ => panic!("status should be String"),
///         }
///     }
///     _ => panic!("result should be Map"),
/// }
/// ```
pub fn resolve_enum_value_recursive(value: &Value, attr_type: &AttributeType) -> Option<Value> {
    // First, try the leaf case: this position is itself an enum.
    if let Some(parts) = attr_type.namespaced_enum_parts()
        && let Some(resolved) = resolve_enum_value(value, &parts)
    {
        return Some(resolved);
    }

    match attr_type {
        AttributeType::Struct { fields, .. } => {
            let Value::Concrete(ConcreteValue::Map(map)) = value else {
                return None;
            };
            let mut rewritten = map.clone();
            let mut changed = false;
            for field in fields {
                if let Some(field_value) = map.get(&field.name)
                    && let Some(new_field) =
                        resolve_enum_value_recursive(field_value, &field.field_type)
                {
                    rewritten.insert(field.name.clone(), new_field);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        AttributeType::List { inner, .. } => {
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return None;
            };
            let mut rewritten = items.clone();
            let mut changed = false;
            for (i, item) in items.iter().enumerate() {
                if let Some(new_item) = resolve_enum_value_recursive(item, inner) {
                    rewritten[i] = new_item;
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::List(rewritten)))
        }
        AttributeType::Map { value: inner, .. } => {
            let Value::Concrete(ConcreteValue::Map(map)) = value else {
                return None;
            };
            let mut rewritten = map.clone();
            let mut changed = false;
            for (k, v) in map {
                if let Some(new_v) = resolve_enum_value_recursive(v, inner) {
                    rewritten.insert(k.clone(), new_v);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        // Scalars and Union: nothing to descend into.
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
    let (type_name, ns, dsl_map) = parts;
    if let Value::Concrete(ConcreteValue::String(s)) = value {
        // Skip values already in namespaced DSL format.
        // A value that contains '.' but is not already namespaced is a raw enum value
        // like "ipsec.1" -- check if it matches a known valid enum value.
        let already_namespaced =
            s.contains('.') && !string_enum_check.is_some_and(|check| check(s));
        if !already_namespaced {
            let dsl_val = dsl_map.dsl_for(s);
            return Some(Value::Concrete(ConcreteValue::String(format!(
                "{}.{}.{}",
                ns, type_name, dsl_val
            ))));
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

/// Resolve a provider config's `region` attribute to an AWS-SDK region
/// string, falling back to `default_region` when the attribute is
/// absent or carried in a shape that is not a region.
///
/// Accepts both quoted-string (`region = "us-east-1"`,
/// `ConcreteValue::String`) and namespaced-identifier (`region =
/// aws.Region.us_east_1`, `ConcreteValue::EnumIdentifier`) shapes —
/// both are how the parser stores the `region` value depending on
/// how the user wrote it. carina#3021 was an instance of this site
/// matching only the `String` arm; namespaced identifiers silently
/// fell through to the caller's hardcoded default region, which
/// silently broke multi-region named provider instances.
///
/// This is the single source of truth for "given a provider config's
/// attribute map, what region string should the SDK use" — every
/// `ProviderFactory::extract_region` implementation should delegate
/// to it.
///
/// # Examples
///
/// ```
/// use carina_core::resource::{ConcreteValue, Value};
/// use carina_core::utils::extract_region_from_attrs;
/// use indexmap::IndexMap;
///
/// let mut attrs = IndexMap::new();
/// attrs.insert(
///     "region".to_string(),
///     Value::Concrete(ConcreteValue::EnumIdentifier("aws.Region.us_east_1".into())),
/// );
/// assert_eq!(extract_region_from_attrs(&attrs, "ap-northeast-1"), "us-east-1");
///
/// let empty: IndexMap<String, Value> = IndexMap::new();
/// assert_eq!(extract_region_from_attrs(&empty, "ap-northeast-1"), "ap-northeast-1");
/// ```
pub fn extract_region_from_attrs(
    attributes: &indexmap::IndexMap<String, crate::resource::Value>,
    default_region: &str,
) -> String {
    use crate::resource::{ConcreteValue, Value};
    match attributes.get("region") {
        Some(Value::Concrete(ConcreteValue::String(s))) => convert_region_value(s),
        Some(Value::Concrete(ConcreteValue::EnumIdentifier(s))) => convert_region_value(s),
        _ => default_region.to_string(),
    }
}

/// Build the canonical address for a map-iteration key — used at every
/// `for ... in <map>` emit site. Identifier-safe keys produce
/// `binding.key`; everything else is single-quoted so the outer
/// `moved` / `removed` string can stay double-quoted without escape
/// juggling. See #1903.
pub fn map_key_address(binding: &str, key: &str) -> String {
    if is_identifier_safe(key) {
        format!("{}.{}", binding, key)
    } else {
        format!("{}['{}']", binding, key)
    }
}

/// Canonicalize the trailing map-key segment of a resource address so
/// `for`-expression iteration over a map and the user-written address
/// in `moved` / `removed` blocks share one shape. See #1903.
///
/// Rules:
/// - `binding["key"]` / `binding['key']` with an identifier-safe key
///   (`[A-Za-z_][A-Za-z0-9_]*`) → `binding.key`.
/// - `binding["key with space"]` (or any non-identifier-safe key) →
///   `binding['key with space']` (single quotes preferred to match the
///   DSL string convention).
/// - `binding[N]` for an integer `N` → unchanged (list index).
/// - Inputs without a trailing `[...]` segment → returned unchanged.
///
/// Only the *last* `[...]` segment is canonicalized — the parser
/// itself only emits a single trailing key per `for`-iteration, so a
/// single-pass rewrite is sufficient.
pub fn canonicalize_map_key_address(name: &str) -> String {
    // Find the final `[`. Nothing to do when the trailing segment isn't
    // a bracket form.
    let Some(open) = name.rfind('[') else {
        return name.to_string();
    };
    if !name.ends_with(']') {
        return name.to_string();
    }
    let prefix = &name[..open];
    let inside = &name[open + 1..name.len() - 1];

    // Numeric index — list iteration. Leave alone.
    if !inside.is_empty() && inside.bytes().all(|b| b.is_ascii_digit()) {
        return name.to_string();
    }

    // Strip surrounding quotes when present so legacy `["key"]` and
    // `['key']` collapse to the same canonical form. The `len < 2`
    // guard keeps `&inside[1..inside.len() - 1]` from panicking when
    // `inside` is a single quote char (e.g. a malformed `binding["]`)
    // — `'"'` satisfies both starts_with and ends_with checks.
    let key = if (inside.starts_with('"') && inside.ends_with('"'))
        || (inside.starts_with('\'') && inside.ends_with('\''))
    {
        if inside.len() < 2 {
            return name.to_string();
        }
        &inside[1..inside.len() - 1]
    } else {
        inside
    };

    if is_identifier_safe(key) {
        format!("{}.{}", prefix, key)
    } else {
        format!("{}['{}']", prefix, key)
    }
}

/// True when `s` is a valid Carina identifier: starts with `[A-Za-z_]`
/// and continues with `[A-Za-z0-9_]*`. The same rule the parser uses
/// for `let` bindings, struct field names, and (after #1903) map keys
/// that can be embedded in a resource address without quoting.
pub fn is_identifier_safe(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Serialize `value` as pretty JSON terminated with `\n`. Matches the
/// trailing-newline convention enforced across all durable JSON
/// artifacts Carina writes (#2583, #2721, #2722, #2754, #2758, #2759)
/// so POSIX tooling and "add final newline" editors agree on the file
/// shape regardless of the writer.
///
/// Each caller maps the returned `serde_json::Error` to its local
/// error type as before; the helper does no error wrapping itself.
pub fn pretty_with_newline<T: serde::Serialize>(value: &T) -> serde_json::Result<String> {
    let mut s = serde_json::to_string_pretty(value)?;
    s.push('\n');
    Ok(s)
}

/// Byte-oriented variant of [`pretty_with_newline`] for callers (e.g.
/// the S3 backend's `PutObject`) that consume `Vec<u8>` directly and
/// want to avoid an intermediate `String`.
pub fn pretty_with_newline_bytes<T: serde::Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut v = serde_json::to_vec_pretty(value)?;
    v.push(b'\n');
    Ok(v)
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
        // 5-part with digit-led tail (`2012_10_17`) — used for IAM policy
        // version identifiers (carina#3051). TypeName at index 3 is preserved.
        assert!(
            validate_enum_namespace(
                "aws.iam.PolicyDocument.Version.2012_10_17",
                "Version",
                "aws.iam.PolicyDocument"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_namespaced_id_parse_numeric_tail() {
        // Digit-led tail with underscores parses through the 5-part shape
        // and flows into `value` verbatim (carina#3051).
        let id =
            NamespacedId::parse("aws.iam.PolicyDocument.Version.2012_10_17").expect("should parse");
        match id {
            NamespacedId::FullyQualified {
                provider,
                segments_str,
                type_name,
                value,
            } => {
                assert_eq!(provider, "aws");
                assert_eq!(segments_str, "iam.PolicyDocument");
                assert_eq!(type_name, "Version");
                assert_eq!(value, "2012_10_17");
            }
            other => panic!("expected FullyQualified, got {:?}", other),
        }
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

    // expand_enum_shorthand pins the contract that `schema.rs` and the LSP
    // `diagnostics` module both rely on. The helper exists specifically so
    // those two paths cannot drift; each row here is a behaviour the unified
    // function must keep on both sides.
    #[test]
    fn expand_enum_shorthand_table() {
        // (input, name, namespace, expected_output_string_or_passthrough)
        // `None` for namespace means "no namespace" — bare and 2-part inputs
        // pass through unchanged.
        let cases: &[(Value, &str, Option<&str>, Value)] = &[
            // bare member + namespace → fully qualified
            (
                Value::Concrete(ConcreteValue::String("dedicated".into())),
                "InstanceTenancy",
                Some("awscc.ec2.Vpc"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // bare member, no namespace → passthrough
            (
                Value::Concrete(ConcreteValue::String("dedicated".into())),
                "InstanceTenancy",
                None,
                Value::Concrete(ConcreteValue::String("dedicated".into())),
            ),
            // TypeName.member matching `name` → fully qualified
            (
                Value::Concrete(ConcreteValue::String("InstanceTenancy.dedicated".into())),
                "InstanceTenancy",
                Some("awscc.ec2.Vpc"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // TypeName.member with foreign type name → passthrough
            (
                Value::Concrete(ConcreteValue::String("Tenancy.dedicated".into())),
                "InstanceTenancy",
                Some("awscc.ec2.Vpc"),
                Value::Concrete(ConcreteValue::String("Tenancy.dedicated".into())),
            ),
            // Already-fully-qualified → passthrough (parser returns
            // `FullyQualified`, helper only acts on `TypeQualified`)
            (
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
                "InstanceTenancy",
                Some("awscc.ec2.Vpc"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // Lowercase first segment → not a TypeName; passthrough
            (
                Value::Concrete(ConcreteValue::String("instanceTenancy.dedicated".into())),
                "InstanceTenancy",
                Some("awscc.ec2.Vpc"),
                Value::Concrete(ConcreteValue::String("instanceTenancy.dedicated".into())),
            ),
            // Non-string → passthrough
            (
                Value::Concrete(ConcreteValue::Bool(true)),
                "Whatever",
                Some("aws"),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
        ];
        for (input, name, ns, expected) in cases {
            let actual = expand_enum_shorthand(input, name, *ns);
            assert_eq!(actual, *expected, "input={input:?} name={name:?} ns={ns:?}",);
        }
    }

    #[test]
    fn canonicalize_drops_quotes_for_identifier_safe_key() {
        // The legacy emit form `["key"]` and the alt form `['key']`
        // both collapse to `binding.key` — see #1903.
        assert_eq!(
            canonicalize_map_key_address("_accounts[\"registry_prod\"]"),
            "_accounts.registry_prod"
        );
        assert_eq!(
            canonicalize_map_key_address("_accounts['registry_prod']"),
            "_accounts.registry_prod"
        );
    }

    #[test]
    fn canonicalize_keeps_already_canonical_dot_form() {
        assert_eq!(
            canonicalize_map_key_address("_accounts.registry_prod"),
            "_accounts.registry_prod"
        );
    }

    #[test]
    fn canonicalize_uses_single_quotes_for_non_identifier_safe_key() {
        // Hyphen, space, leading digit, and dot are all non-safe — the
        // canonical form keeps them in single-quoted brackets so the
        // outer `moved`/`removed` string can stay double-quoted without
        // escape juggling.
        assert_eq!(
            canonicalize_map_key_address("_envs[\"prod-east\"]"),
            "_envs['prod-east']"
        );
        assert_eq!(
            canonicalize_map_key_address("_envs[\"key with space\"]"),
            "_envs['key with space']"
        );
        assert_eq!(
            canonicalize_map_key_address("_envs[\"3rd-region\"]"),
            "_envs['3rd-region']"
        );
        assert_eq!(
            canonicalize_map_key_address("_envs[\"a.b\"]"),
            "_envs['a.b']"
        );
    }

    #[test]
    fn canonicalize_leaves_numeric_list_index_unchanged() {
        assert_eq!(canonicalize_map_key_address("_accounts[0]"), "_accounts[0]");
        assert_eq!(
            canonicalize_map_key_address("_accounts[42]"),
            "_accounts[42]"
        );
    }

    #[test]
    fn canonicalize_returns_input_when_no_trailing_bracket() {
        assert_eq!(canonicalize_map_key_address("my_bucket"), "my_bucket");
        assert_eq!(
            canonicalize_map_key_address("explicit-name-with-hyphens"),
            "explicit-name-with-hyphens"
        );
    }

    #[test]
    fn pretty_with_newline_appends_single_trailing_newline() {
        let value = serde_json::json!({"a": 1});
        let s = pretty_with_newline(&value).unwrap();
        assert!(s.ends_with("}\n"), "expected `...}}\\n`, got: {s:?}");
        // Exactly one newline, not two.
        assert!(!s.ends_with("\n\n"));
    }

    #[test]
    fn pretty_with_newline_bytes_appends_single_trailing_newline() {
        let value = serde_json::json!({"a": 1});
        let v = pretty_with_newline_bytes(&value).unwrap();
        assert_eq!(v.last().copied(), Some(b'\n'));
        let len = v.len();
        // Last char is `\n`, second-to-last is `}` — not a double newline.
        assert_eq!(v.get(len - 2).copied(), Some(b'}'));
    }

    #[test]
    fn pretty_with_newline_propagates_serialize_error() {
        // serde_json refuses non-string keys in maps because the JSON
        // spec only allows string keys. A `BTreeMap<Vec<i32>, _>` key
        // would serialize to a JSON array, so the serializer fails.
        // The helper must surface that error, not swallow it.
        let mut map = std::collections::BTreeMap::new();
        map.insert(vec![1, 2], "x");
        let err = pretty_with_newline(&map).unwrap_err();
        // The exact message is serde_json's; we only assert that a
        // serialization failure surfaces as an Err.
        assert!(!err.to_string().is_empty());
    }

    mod resolve_enum_value_recursive {
        use super::*;
        use crate::schema::StructField;
        use indexmap::IndexMap;

        fn versioning_status() -> AttributeType {
            AttributeType::StringEnum {
                name: "VersioningStatus".to_string(),
                values: vec!["Enabled".to_string(), "Suspended".to_string()],
                namespace: Some("aws.s3.Bucket".to_string()),
                dsl_aliases: vec![],
            }
        }

        fn s(s: &str) -> Value {
            Value::Concrete(ConcreteValue::String(s.to_string()))
        }

        fn ei(s: &str) -> Value {
            Value::Concrete(ConcreteValue::EnumIdentifier(s.to_string()))
        }

        /// Regression for aws#313: post-carina#2986 the parser emits
        /// bare DSL enum values as `EnumIdentifier`, not `String`.
        /// `resolve_enum_value` must resolve them identically (and
        /// materialize the result as the fully-qualified `String`
        /// form), otherwise bare struct-field enums like
        /// `effect = allow` are left unresolved and diverge from the
        /// AWS-read side which produces the namespaced spelling.
        #[test]
        fn leaf_enum_identifier_resolves_like_string() {
            let aliased = AttributeType::StringEnum {
                name: "Effect".to_string(),
                values: vec!["Allow".to_string(), "Deny".to_string()],
                namespace: Some("aws.iam.PolicyDocument".to_string()),
                dsl_aliases: vec![
                    ("Allow".to_string(), "allow".to_string()),
                    ("Deny".to_string(), "deny".to_string()),
                ],
            };
            // Bare EnumIdentifier resolves to the fully-qualified String
            // form — identical to what the String input would produce.
            let from_ei = resolve_enum_value_recursive(&ei("allow"), &aliased).unwrap();
            assert_eq!(from_ei, s("aws.iam.PolicyDocument.Effect.allow"));
            let from_str = resolve_enum_value_recursive(&s("allow"), &aliased).unwrap();
            assert_eq!(from_ei, from_str);

            // TypeName.value shorthand as an EnumIdentifier resolves too.
            let from_tn = resolve_enum_value_recursive(&ei("Effect.deny"), &aliased).unwrap();
            assert_eq!(from_tn, s("aws.iam.PolicyDocument.Effect.deny"));
        }

        #[test]
        fn leaf_string_enum_resolves_bare_value() {
            let resolved = resolve_enum_value_recursive(&s("Enabled"), &versioning_status());
            assert_eq!(resolved, Some(s("aws.s3.Bucket.VersioningStatus.Enabled")));
        }

        #[test]
        fn non_enum_scalar_passes_through() {
            let resolved = resolve_enum_value_recursive(&s("hello"), &AttributeType::String);
            assert_eq!(resolved, None);
        }

        #[test]
        fn struct_with_enum_field_resolves_field() {
            let config = AttributeType::Struct {
                name: "VersioningConfiguration".to_string(),
                fields: vec![StructField::new("status", versioning_status())],
            };
            let mut inner = IndexMap::new();
            inner.insert("status".to_string(), s("Enabled"));
            let input = Value::Concrete(ConcreteValue::Map(inner));

            let resolved = resolve_enum_value_recursive(&input, &config).unwrap();
            match resolved {
                Value::Concrete(ConcreteValue::Map(m)) => {
                    assert_eq!(
                        m.get("status"),
                        Some(&s("aws.s3.Bucket.VersioningStatus.Enabled"))
                    );
                }
                _ => panic!("expected Map"),
            }
        }

        #[test]
        fn struct_with_no_enum_changes_returns_none() {
            let config = AttributeType::Struct {
                name: "Config".to_string(),
                fields: vec![StructField::new("name", AttributeType::String)],
            };
            let mut inner = IndexMap::new();
            inner.insert("name".to_string(), s("foo"));
            let input = Value::Concrete(ConcreteValue::Map(inner));

            assert_eq!(resolve_enum_value_recursive(&input, &config), None);
        }

        #[test]
        fn list_of_enum_resolves_every_item() {
            let list_t = AttributeType::List {
                inner: Box::new(versioning_status()),
                ordered: true,
            };
            let input = Value::Concrete(ConcreteValue::List(vec![s("Enabled"), s("Suspended")]));
            let resolved = resolve_enum_value_recursive(&input, &list_t).unwrap();
            match resolved {
                Value::Concrete(ConcreteValue::List(items)) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0], s("aws.s3.Bucket.VersioningStatus.Enabled"));
                    assert_eq!(items[1], s("aws.s3.Bucket.VersioningStatus.Suspended"));
                }
                _ => panic!("expected List"),
            }
        }

        #[test]
        fn map_of_enum_resolves_every_value() {
            let map_t = AttributeType::Map {
                key: Box::new(AttributeType::String),
                value: Box::new(versioning_status()),
            };
            let mut input_map = IndexMap::new();
            input_map.insert("primary".to_string(), s("Enabled"));
            input_map.insert("secondary".to_string(), s("Suspended"));
            let input = Value::Concrete(ConcreteValue::Map(input_map));

            let resolved = resolve_enum_value_recursive(&input, &map_t).unwrap();
            match resolved {
                Value::Concrete(ConcreteValue::Map(m)) => {
                    assert_eq!(
                        m.get("primary"),
                        Some(&s("aws.s3.Bucket.VersioningStatus.Enabled"))
                    );
                    assert_eq!(
                        m.get("secondary"),
                        Some(&s("aws.s3.Bucket.VersioningStatus.Suspended"))
                    );
                }
                _ => panic!("expected Map"),
            }
        }

        #[test]
        fn list_of_struct_with_enum_field_descends_into_each_item() {
            // List<Struct{status: VersioningStatus}>
            let item_t = AttributeType::Struct {
                name: "Rule".to_string(),
                fields: vec![StructField::new("status", versioning_status())],
            };
            let list_t = AttributeType::List {
                inner: Box::new(item_t),
                ordered: false,
            };
            let mut item1 = IndexMap::new();
            item1.insert("status".to_string(), s("Enabled"));
            let mut item2 = IndexMap::new();
            item2.insert("status".to_string(), s("Suspended"));
            let input = Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Map(item1)),
                Value::Concrete(ConcreteValue::Map(item2)),
            ]));

            let resolved = resolve_enum_value_recursive(&input, &list_t).unwrap();
            let Value::Concrete(ConcreteValue::List(items)) = resolved else {
                panic!("expected List");
            };
            for (item, expected) in items.iter().zip(
                [
                    "aws.s3.Bucket.VersioningStatus.Enabled",
                    "aws.s3.Bucket.VersioningStatus.Suspended",
                ]
                .iter(),
            ) {
                let Value::Concrete(ConcreteValue::Map(m)) = item else {
                    panic!("expected Map");
                };
                assert_eq!(m.get("status"), Some(&s(expected)));
            }
        }

        #[test]
        fn nested_struct_descends_recursively() {
            // Struct{outer_field: Struct{status: VersioningStatus}}
            let inner_t = AttributeType::Struct {
                name: "Inner".to_string(),
                fields: vec![StructField::new("status", versioning_status())],
            };
            let outer_t = AttributeType::Struct {
                name: "Outer".to_string(),
                fields: vec![StructField::new("inner", inner_t)],
            };

            let mut inner_map = IndexMap::new();
            inner_map.insert("status".to_string(), s("Enabled"));
            let mut outer_map = IndexMap::new();
            outer_map.insert(
                "inner".to_string(),
                Value::Concrete(ConcreteValue::Map(inner_map)),
            );
            let input = Value::Concrete(ConcreteValue::Map(outer_map));

            let resolved = resolve_enum_value_recursive(&input, &outer_t).unwrap();
            let Value::Concrete(ConcreteValue::Map(om)) = resolved else {
                panic!("expected Map");
            };
            let Some(Value::Concrete(ConcreteValue::Map(im))) = om.get("inner") else {
                panic!("expected nested Map");
            };
            assert_eq!(
                im.get("status"),
                Some(&s("aws.s3.Bucket.VersioningStatus.Enabled"))
            );
        }

        #[test]
        fn already_qualified_value_is_unchanged() {
            // Fully-qualified DSL form should pass through unchanged
            // (resolve_enum_value's contract — we exercise it through
            // the recursive entry point to confirm passthrough
            // propagates to None at this level too).
            let resolved = resolve_enum_value_recursive(
                &s("aws.s3.Bucket.VersioningStatus.Enabled"),
                &versioning_status(),
            );
            assert_eq!(resolved, None);
        }

        #[test]
        fn dsl_alias_resolves_via_dsl_for() {
            // Enum with a DSL alias: dsl_aliases maps API "Enabled" → DSL "enabled".
            // resolve_enum_value calls dsl_for(api) to render the alias,
            // so the resolved fully-qualified form uses the DSL spelling.
            let aliased = AttributeType::StringEnum {
                name: "VersioningStatus".to_string(),
                values: vec!["Enabled".to_string()],
                namespace: Some("aws.s3.Bucket".to_string()),
                dsl_aliases: vec![("Enabled".to_string(), "enabled".to_string())],
            };
            let resolved = resolve_enum_value_recursive(&s("Enabled"), &aliased).unwrap();
            assert_eq!(resolved, s("aws.s3.Bucket.VersioningStatus.enabled"));
        }

        #[test]
        fn map_of_struct_with_enum_field_descends() {
            // Map<String, Struct{status: VersioningStatus}>
            let item_t = AttributeType::Struct {
                name: "Rule".to_string(),
                fields: vec![StructField::new("status", versioning_status())],
            };
            let map_t = AttributeType::Map {
                key: Box::new(AttributeType::String),
                value: Box::new(item_t),
            };
            let mut item = IndexMap::new();
            item.insert("status".to_string(), s("Enabled"));
            let mut input_map = IndexMap::new();
            input_map.insert("a".to_string(), Value::Concrete(ConcreteValue::Map(item)));
            let input = Value::Concrete(ConcreteValue::Map(input_map));

            let resolved = resolve_enum_value_recursive(&input, &map_t).unwrap();
            let Value::Concrete(ConcreteValue::Map(om)) = resolved else {
                panic!("expected Map");
            };
            let Some(Value::Concrete(ConcreteValue::Map(im))) = om.get("a") else {
                panic!("expected nested Map");
            };
            assert_eq!(
                im.get("status"),
                Some(&s("aws.s3.Bucket.VersioningStatus.Enabled"))
            );
        }

        #[test]
        fn union_is_not_recursed_into() {
            // Union types are documented as not recursed; assert by
            // construction that a Union wrapping an enum yields None
            // even when the value would resolve for the enum directly.
            let union_t = AttributeType::Union(vec![versioning_status(), AttributeType::String]);
            assert_eq!(resolve_enum_value_recursive(&s("Enabled"), &union_t), None);
        }
    }

    /// carina#3021: `extract_region_from_attrs` is the canonical answer
    /// to "what SDK region does this provider config want". Every
    /// `ProviderFactory::extract_region` implementation should
    /// delegate to it, so a single match-arm coverage test here is
    /// what protects every downstream caller.
    mod extract_region_from_attrs_tests {
        use super::super::extract_region_from_attrs;
        use crate::resource::{ConcreteValue, Value};
        use indexmap::IndexMap;

        fn attrs_with_region(value: Value) -> IndexMap<String, Value> {
            let mut m = IndexMap::new();
            m.insert("region".to_string(), value);
            m
        }

        #[test]
        fn enum_identifier_namespaced_form() {
            let attrs = attrs_with_region(Value::Concrete(ConcreteValue::EnumIdentifier(
                "aws.Region.us_east_1".to_string(),
            )));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "us-east-1"
            );
        }

        #[test]
        fn string_aws_sdk_form() {
            let attrs = attrs_with_region(Value::Concrete(ConcreteValue::String(
                "us-east-1".to_string(),
            )));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "us-east-1"
            );
        }

        #[test]
        fn string_namespaced_form() {
            // Users sometimes write `region = "aws.Region.us_east_1"` —
            // quoted-string spelling. The canonicalizer must still
            // reduce it to the SDK form, matching the EnumIdentifier
            // path.
            let attrs = attrs_with_region(Value::Concrete(ConcreteValue::String(
                "aws.Region.us_east_1".to_string(),
            )));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "us-east-1"
            );
        }

        #[test]
        fn missing_region_returns_default() {
            let attrs: IndexMap<String, Value> = IndexMap::new();
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "ap-northeast-1"
            );
        }
    }
}
