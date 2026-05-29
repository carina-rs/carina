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

    /// Match this parsed identifier against the expected
    /// [`TypeIdentity`].
    ///
    /// The structured form unifies the old `(namespace, type_name)` pair
    /// into a single axis-bearing record. Under the new value-name-space
    /// convention (`{provider}.{segments...}.{kind}.{value}`), the
    /// identifier's `type_name` must equal the identity's `kind`, the
    /// `provider` segments must match, and `segments_str` must match the
    /// identity's `segments` joined with `.`.
    pub fn matches_identity(&self, expected: &crate::schema::TypeIdentity) -> bool {
        match self {
            // 2-part `TypeName.value` shorthand matches when the leading
            // TypeName equals the identity's kind. The provider axis is
            // not yet known at this point — the caller decides whether
            // the shorthand is acceptable in context.
            Self::TypeQualified { type_name, .. } => *type_name == expected.kind,
            Self::ProviderQualified {
                provider,
                type_name,
                ..
            } => {
                // 3-part `provider.TypeName.value` matches only when the
                // expected identity has no `segments` (a provider-scoped
                // bare-kind type like `aws.Region`).
                expected.segments.is_empty()
                    && expected.provider.as_deref() == Some(*provider)
                    && *type_name == expected.kind
            }
            Self::FullyQualified {
                provider,
                segments_str,
                type_name,
                ..
            } => {
                if *type_name != expected.kind {
                    return false;
                }
                if expected.provider.as_deref() != Some(*provider) {
                    return false;
                }
                // Compare the parsed `segments_str` to the identity's
                // `segments` slice without allocating a Vec.
                let mut expected_iter = expected.segments.iter().map(String::as_str);
                let mut actual_iter = segments_str.split('.');
                loop {
                    match (expected_iter.next(), actual_iter.next()) {
                        (Some(a), Some(b)) if a == b => continue,
                        (None, None) => return true,
                        _ => return false,
                    }
                }
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
/// `identity` is the structured [`crate::schema::TypeIdentity`] of the
/// receiving attribute; the expanded form is the identity's dotted
/// display followed by the value — `aws.iam.Role.Arn.<v>` for a
/// fully-segmented identity, `aws.Region.<v>` for a bare-segment one,
/// `aws.AvailabilityZone.ZoneName.<v>` for the segments + kind case
/// users now write. The 2-part `TypeName.member` shorthand is
/// recognised when `TypeName` matches the identity's enum type name
/// (its last segment, or the kind if segments are empty).
///
/// Accepts the three input shapes the DSL allows for `StringEnum` and
/// enum-like `Custom` attributes:
///
/// - bare member (`dedicated`) → `{identity}.dedicated`
/// - `TypeName.member` shorthand (`InstanceTenancy.dedicated`) →
///   `{identity}.dedicated`, only when the type name matches the
///   identity's enum type name
/// - any other input (already-qualified, foreign type name, identity
///   with no provider axis, non-string) → returned unchanged
///
/// Used by both `AttributeType::resolve_value` and the LSP diagnostic
/// pipeline so the two paths cannot drift.
pub fn expand_enum_shorthand(value: &Value, identity: &crate::schema::TypeIdentity) -> Value {
    // Phase 4 of carina#2986: `EnumIdentifier` carries the same textual
    // payload as `String` (only the source-shape tag differs) and goes
    // through the same namespace expansion. The result is materialized as
    // `String` because every downstream consumer
    // (`validate_string_enum`, the LSP completion helpers, the
    // builtin-result coercion in `convert_enum_value`) reads through the
    // `Value::Concrete(ConcreteValue::String)` arm. The `EnumIdentifier`
    // distinction is a parser-level signal used for strict shape
    // enforcement at the validator entry, not a wire-level form.
    //
    // The expanded form is the dotted display of the structured
    // identity followed by the value: `{provider}.{segments...}.{kind}.{value}`
    // — same shape as the type's `TypeIdentity::Display`. For a bare
    // identity with no provider axis the shorthand passes through
    // unchanged (no namespace to prefix).
    let text_form: Option<&str> = match value {
        Value::Concrete(ConcreteValue::String(s)) => Some(s.as_str()),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Some(s.as_str()),
        _ => None,
    };
    // The kind is the type's own name and matches the leading
    // `TypeName` of the 2-part `TypeName.value` shorthand:
    // `InstanceTenancy.dedicated` against a Custom whose kind is
    // `InstanceTenancy`, or `ZoneName.us_east_1a` against the
    // zone-name AvailabilityZone type.
    let enum_type_name: &str = &identity.kind;
    match text_form {
        Some(s) if !s.contains('.') => {
            if identity.provider.is_some() {
                Value::Concrete(ConcreteValue::String(format!("{}.{}", identity, s)))
            } else {
                Value::Concrete(ConcreteValue::String(s.to_string()))
            }
        }
        Some(s) => {
            if let Some(NamespacedId::TypeQualified {
                type_name: ident,
                value: member,
            }) = NamespacedId::parse(s)
                && identity.provider.is_some()
                && ident == enum_type_name
            {
                Value::Concrete(ConcreteValue::String(format!("{}.{}", identity, member)))
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

/// Canonicalize one `StringEnum` value to its API spelling: strip any
/// namespace prefix ([`extract_enum_value_with_values`]) then map the
/// DSL alias to the API form via [`crate::schema::DslMap::api_for`].
///
/// This is the exact-match canonicalization used by the plan renderer's
/// `StringEnum`-list diff (carina#3075) and the carina-provider
/// `api_canonicalize` normalizers. (The differ's `StringEnum` equality
/// arm uses a deliberately *case-insensitive* alias fold for comparison
/// — a different operation — and is intentionally not routed here.)
///
/// ```
/// use carina_core::schema::DslMap;
/// use carina_core::utils::canonicalize_enum_to_api;
///
/// let aliases = [("Allow".to_string(), "allow".to_string())];
/// let dsl_map = DslMap::Aliases(&aliases);
/// let valid = &["Allow", "Deny"];
/// // DSL alias → API spelling
/// assert_eq!(canonicalize_enum_to_api("allow", valid, &dsl_map), "Allow");
/// // Already-canonical round-trips to itself
/// assert_eq!(canonicalize_enum_to_api("Allow", valid, &dsl_map), "Allow");
/// // Namespaced form is stripped, then canonicalized
/// assert_eq!(
///     canonicalize_enum_to_api("aws.x.Mode.allow", valid, &dsl_map),
///     "Allow"
/// );
/// ```
pub fn canonicalize_enum_to_api(
    s: &str,
    valid_values: &[&str],
    dsl_map: &crate::schema::DslMap<'_>,
) -> String {
    dsl_map.api_for(extract_enum_value_with_values(s, valid_values))
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
/// The expected full form is the identity's dotted display followed by
/// a value: for `aws.Region` (identity with no segments) the full form
/// is `aws.Region.value`; for `aws.AvailabilityZone.ZoneName` the full
/// form is `aws.AvailabilityZone.ZoneName.us_east_1a`.
///
/// Callers that need to accept enum values containing dots (e.g.,
/// `"ipsec.1"`) must handle that case themselves before calling this
/// function.
///
/// # Arguments
/// * `s` - The input string to validate
/// * `identity` - The receiving attribute's [`crate::schema::TypeIdentity`]
///
/// # Returns
/// * `Ok(())` if namespace is valid or string has no dots
/// * `Err(String)` with bare reason string (without the input value) if namespace is invalid
///
/// # Examples
///
/// ```
/// use carina_core::schema::TypeIdentity;
/// use carina_core::utils::validate_enum_namespace;
///
/// let region = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region");
/// let bucket_ver = TypeIdentity::new(Some("aws"), ["s3", "Bucket"], "VersioningStatus");
///
/// // No dots — passes through
/// assert!(validate_enum_namespace("Enabled", &bucket_ver).is_ok());
///
/// // 2-part: TypeName.value (TypeName must equal `identity.kind`)
/// assert!(validate_enum_namespace("Region.ap_northeast_1", &region).is_ok());
/// assert!(validate_enum_namespace("Location.ap_northeast_1", &region).is_err());
///
/// // Full namespaced form: equals `{identity}.<value>`
/// assert!(validate_enum_namespace("aws.Region.ap_northeast_1", &region).is_ok());
/// assert!(validate_enum_namespace("aws.s3.Bucket.VersioningStatus.Enabled", &bucket_ver).is_ok());
/// ```
pub fn validate_enum_namespace(
    s: &str,
    identity: &crate::schema::TypeIdentity,
) -> Result<(), String> {
    if !s.contains('.') {
        return Ok(());
    }

    // The full form has one segment per identity axis plus the value:
    // identity provider + segments + kind + value.
    let prefix = identity.to_string();
    let actual_parts = s.split('.').count();
    let expected_full_len = prefix.split('.').count() + 1;
    let is_two_part = actual_parts == 2;
    let is_full_form = actual_parts == expected_full_len;
    if !is_two_part && !is_full_form {
        return Err(format!(
            "expected format: value, {}.value, or {}.value",
            identity.kind, prefix
        ));
    }

    if let Some(id) = NamespacedId::parse(s)
        && id.matches_identity(identity)
    {
        return Ok(());
    }
    // Mirror the original error-message shape: 2-part inputs get the
    // "or full form" hint, full-form inputs get only the full form.
    if is_two_part {
        Err(format!(
            "expected format {}.value or {}.value",
            identity.kind, prefix
        ))
    } else {
        Err(format!("expected format {}.value", prefix))
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
///     identity: Some(carina_core::schema::string_enum_identity("VersioningStatus", Some("aws.s3.Bucket"))),
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
    resolve_enum_value_recursive_with_defs(value, attr_type, crate::schema::empty_defs())
}

/// Same as [`resolve_enum_value_recursive`] but takes the enclosing
/// [`ResourceSchema::defs`] map so cyclic CFN definitions
/// (`AttributeType::Ref`) can be followed during the type walk
/// (carina#3340). Callers that hold a resource schema should prefer
/// this entry point so a `Ref` chain inside a value tree (e.g. WAFv2
/// `WebACL.Statement -> AndStatement -> List<Statement>`) doesn't
/// silently lose enum normalization on the recursed cycle.
pub fn resolve_enum_value_recursive_with_defs(
    value: &Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Option<Value> {
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
                        resolve_enum_value_recursive_with_defs(field_value, &field.field_type, defs)
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
                if let Some(new_item) = resolve_enum_value_recursive_with_defs(item, inner, defs) {
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
                if let Some(new_v) = resolve_enum_value_recursive_with_defs(v, inner, defs) {
                    rewritten.insert(k.clone(), new_v);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        // `Ref`: follow the named target in the schema's def map and
        // continue the walk. Without this arm a cyclic schema would
        // silently drop enum normalization at every cycle point
        // (carina#3340). `resolve_refs` panics on a missing def name
        // (schema invariant violation).
        AttributeType::Ref(_) => {
            let resolved = attr_type.resolve_refs(defs);
            resolve_enum_value_recursive_with_defs(value, resolved.as_attr(), defs)
        }
        // Scalars and Union: nothing to descend into.
        _ => None,
    }
}

/// Lift every `ConcreteValue::String` that sits at a `StringEnum`-typed
/// position to `ConcreteValue::EnumIdentifier`, descending into struct
/// fields, list elements, and map values, when (and only when) the
/// string is a recognized member of that enum.
///
/// # Why this exists (awscc#251)
///
/// carina#2986 Phase 4 made the schema validator *strict*: a
/// `StringEnum` position only accepts `ConcreteValue::EnumIdentifier`,
/// never `ConcreteValue::String` (a bare quoted string at an enum
/// position is a form error — see the `StringLiteralExpectedEnum`
/// diagnostic). awscc#250 then promoted IAM policy `version`/`effect`
/// from `Custom` to `StringEnum`.
///
/// Persisted state files (S3/local JSON) written *before* that schema
/// change store these values as plain JSON strings. On load the
/// schema-blind bridge `json_to_dsl_value` (carina-state) turns them
/// into `ConcreteValue::String`. `carina validate`/`plan` then
/// re-validate upstream-state-referenced resources against the *new*
/// schema, and the loaded `String` is rejected at the now-`StringEnum`
/// position even though the stored value is perfectly valid.
///
/// This function is the read/state-load counterpart to
/// [`resolve_enum_value_recursive`] (which handles the desired-side,
/// parser-fed direction). It is intentionally schema-aware and general:
/// it does not name IAM, `version`, or `effect` anywhere, so any
/// current or future `Custom`→`StringEnum` migration is covered without
/// further changes. It is *not* a validator carve-out — the validator
/// stays strict; old state is migrated in memory before validation.
///
/// Only recognized members are lifted. A string is recognized when it
/// equals one of the enum's API-canonical `values` or appears on either
/// side of a `dsl_aliases` pair. Unrecognized strings are left as
/// `ConcreteValue::String` so the strict validator still rejects
/// genuinely-invalid state instead of silently masking it.
///
/// The variant tag changes from `String` to `EnumIdentifier` (mirroring
/// the carina#2996 map-key precedent in `AttributeType::validate_map`,
/// schema/mod.rs). The text is normalized to the **DSL spelling** via
/// `DslMap::dsl_for` because the strict validator rejects the
/// API-canonical form whenever a `dsl_aliases` entry rewrites it (the
/// DSL surface convention is the alias spelling — see
/// `feedback_dsl_enum_snake_case_convention.md` and carina#2980). For
/// enums with no rewriting alias the DSL spelling equals the API
/// spelling, so the text is unchanged in that case.
///
/// `Union` types are not recursed into, matching
/// [`resolve_enum_value_recursive`]'s documented contract.
pub fn lift_state_string_enums_to_identifiers(
    attributes: &mut std::collections::HashMap<String, Value>,
    schema: &crate::schema::ResourceSchema,
) {
    for (name, attr) in &schema.attributes {
        if let Some(value) = attributes.get(name)
            && let Some(lifted) =
                lift_string_enum_leaves_with_defs(value, &attr.attr_type, &schema.defs)
        {
            attributes.insert(name.clone(), lifted);
        }
    }
}

/// Apply [`lift_state_string_enums_to_identifiers`] to every resource's
/// loaded prior-state attributes, resolving each resource's schema from
/// `registry`.
///
/// This is the single entry point every persisted-state load seam must
/// call before the state reaches the differ or the validator. There are
/// three such seams in the CLI — `carina plan`, `carina apply`, and
/// `carina state` — each builds its own `saved_attrs` map from
/// `StateFile::build_saved_attrs`. Wiring this helper at all three keeps
/// the migration uniform: fixing only the plan seam (awscc#251's first
/// cut) left `apply` carrying un-lifted `String` state, which the
/// already-lifted desired side then diffed against as a spurious
/// `String` vs `EnumIdentifier` change on every apply.
///
/// Resources whose schema is not in `registry` (or that have no saved
/// attributes) are skipped.
pub fn lift_saved_state_string_enums(
    saved_attrs: &mut std::collections::HashMap<
        crate::resource::ResourceId,
        std::collections::HashMap<String, Value>,
    >,
    resources: &[crate::resource::Resource],
    registry: &crate::schema::SchemaRegistry,
) {
    for resource in resources {
        if let Some(schema) = registry.get_for(resource)
            && let Some(attrs) = saved_attrs.get_mut(&resource.id)
        {
            lift_state_string_enums_to_identifiers(attrs, schema);
        }
    }
}

/// Apply [`lift_state_string_enums_to_identifiers`] to every resource's
/// **read-back** state attributes (`current_states`), resolving each
/// resource's schema from `registry`.
///
/// [`lift_saved_state_string_enums`] only migrates the cached
/// `saved_attrs` map (state-file JSON). On a refresh, the live value is
/// produced by `provider.read()` and lands in `current_states`, a
/// *different* map the saved-attrs lift never touches. A provider that
/// returns an IAM policy document with plain `String` `version` /
/// `effect` (the on-the-wire shape for a field that was `Custom` when
/// the resource was created, now `StringEnum` after awscc#250) then
/// flows un-lifted into the differ, where the strict carina#2986
/// validator rejects it — the exact failure awscc#251's first cut
/// (#3055) did not fix because it only covered `saved_attrs`. Call this
/// once after both refresh branches have populated `current_states`,
/// before the differ / resolver consume it.
///
/// Resources whose schema is not in `registry` (or that have no state)
/// are skipped.
pub fn lift_current_state_string_enums(
    current_states: &mut std::collections::HashMap<
        crate::resource::ResourceId,
        crate::resource::State,
    >,
    resources: &[crate::resource::Resource],
    registry: &crate::schema::SchemaRegistry,
) {
    for resource in resources {
        if let Some(schema) = registry.get_for(resource)
            && let Some(state) = current_states.get_mut(&resource.id)
        {
            lift_state_string_enums_to_identifiers(&mut state.attributes, schema);
        }
    }
}

/// [`DataSource`](crate::resource::DataSource) counterpart of
/// [`lift_current_state_string_enums`]. Schema lookup routes through the
/// data-source registry (`get_for_data_source`) so a `read` resource's
/// provider-returned state is StringEnum-lifted too (carina#3181).
pub fn lift_current_state_string_enums_for_data_sources(
    current_states: &mut std::collections::HashMap<
        crate::resource::ResourceId,
        crate::resource::State,
    >,
    data_sources: &[crate::resource::DataSource],
    registry: &crate::schema::SchemaRegistry,
) {
    for data_source in data_sources {
        if let Some(schema) = registry.get_for_data_source(data_source)
            && let Some(state) = current_states.get_mut(&data_source.id)
        {
            lift_state_string_enums_to_identifiers(&mut state.attributes, schema);
        }
    }
}

/// Value-level worker for [`lift_state_string_enums_to_identifiers`].
///
/// Returns `Some(new_value)` when at least one nested value was lifted,
/// `None` when nothing changed — mirroring
/// [`resolve_enum_value_recursive`]'s "rewrite only on diff" contract so
/// callers can skip cloning.
pub fn lift_string_enum_leaves(value: &Value, attr_type: &AttributeType) -> Option<Value> {
    lift_string_enum_leaves_with_defs(value, attr_type, crate::schema::empty_defs())
}

/// Same as [`lift_string_enum_leaves`] but takes the enclosing
/// [`ResourceSchema::defs`] map so cyclic CFN definitions
/// (`AttributeType::Ref`) are followed during the value walk
/// (carina#3340). Callers that hold a resource schema should prefer
/// this entry point.
pub fn lift_string_enum_leaves_with_defs(
    value: &Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Option<Value> {
    // Leaf case: this position is itself a StringEnum.
    if let Some((_, values, _, dsl_map)) = attr_type.string_enum_parts() {
        if let Value::Concrete(ConcreteValue::String(s)) = value {
            let is_member = values.iter().any(|v| v == s)
                || match dsl_map {
                    crate::schema::DslMap::Aliases(pairs) => {
                        pairs.iter().any(|(api, dsl)| api == s || dsl == s)
                    }
                    crate::schema::DslMap::Closure(_) => false,
                };
            if is_member {
                // Normalize to the DSL spelling: the strict validator
                // requires the alias form when one rewrites the API
                // value. `dsl_for` is identity for non-aliased members
                // and idempotent when the stored value is already the
                // DSL spelling.
                let dsl = dsl_map.dsl_for(s);
                return Some(Value::Concrete(ConcreteValue::EnumIdentifier(dsl)));
            }
        }
        // Recognized-or-not, a StringEnum leaf has no children.
        return None;
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
                        lift_string_enum_leaves_with_defs(field_value, &field.field_type, defs)
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
                if let Some(new_item) = lift_string_enum_leaves_with_defs(item, inner, defs) {
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
                if let Some(new_v) = lift_string_enum_leaves_with_defs(v, inner, defs) {
                    rewritten.insert(k.clone(), new_v);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        // `Ref`: resolve via defs and continue. See sibling note in
        // `resolve_enum_value_recursive_with_defs` (carina#3340).
        AttributeType::Ref(_) => {
            let resolved = attr_type.resolve_refs(defs);
            lift_string_enum_leaves_with_defs(value, resolved.as_attr(), defs)
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
    use crate::schema::TypeIdentity;

    /// Build a `TypeIdentity` from the legacy `(name, namespace)` pair
    /// the pre-S2.5b test corpus used. Provider is the first segment of
    /// `namespace`; the remainder become the structured segments; the
    /// enum type name becomes the kind. Keeps the existing assertions
    /// readable while the call sites migrate to the structured form.
    fn legacy_identity(name: &str, namespace: &str) -> TypeIdentity {
        let mut parts = namespace.split('.');
        let provider = parts.next().map(String::from);
        let segments: Vec<String> = parts.map(String::from).collect();
        TypeIdentity {
            provider,
            segments,
            kind: name.to_string(),
        }
    }

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
        assert!(
            validate_enum_namespace(
                "Enabled",
                &legacy_identity("VersioningStatus", "aws.s3.Bucket")
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace("ap-northeast-1", &legacy_identity("Region", "aws")).is_ok()
        );
        assert!(
            validate_enum_namespace(
                "default",
                &legacy_identity("InstanceTenancy", "awscc.ec2.Vpc")
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_2_part_valid() {
        assert!(
            validate_enum_namespace("Region.ap_northeast_1", &legacy_identity("Region", "aws"))
                .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "VersioningStatus.Enabled",
                &legacy_identity("VersioningStatus", "aws.s3.Bucket")
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "InstanceTenancy.default",
                &legacy_identity("InstanceTenancy", "awscc.ec2.Vpc")
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_2_part_invalid() {
        assert!(
            validate_enum_namespace("Location.ap_northeast_1", &legacy_identity("Region", "aws"))
                .is_err()
        );
        assert!(
            validate_enum_namespace(
                "Versioning.Enabled",
                &legacy_identity("VersioningStatus", "aws.s3.Bucket")
            )
            .is_err()
        );
        assert!(
            validate_enum_namespace(
                "Tenancy.default",
                &legacy_identity("InstanceTenancy", "awscc.ec2.Vpc")
            )
            .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_3_part_valid() {
        // 3-part is valid for 1-segment namespace (e.g., "aws")
        assert!(
            validate_enum_namespace(
                "aws.Region.ap_northeast_1",
                &legacy_identity("Region", "aws")
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_3_part_invalid() {
        // Wrong provider
        assert!(
            validate_enum_namespace(
                "gcp.Region.ap_northeast_1",
                &legacy_identity("Region", "aws")
            )
            .is_err()
        );
        // Wrong type name
        assert!(
            validate_enum_namespace(
                "aws.Location.ap_northeast_1",
                &legacy_identity("Region", "aws")
            )
            .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_4_part_valid() {
        // 4-part is valid for 2-segment namespace (e.g., "aws.s3")
        assert!(
            validate_enum_namespace(
                "aws.s3.VersioningStatus.Enabled",
                &legacy_identity("VersioningStatus", "aws.s3")
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
                &legacy_identity("VersioningStatus", "aws.s3")
            )
            .is_err()
        );
        // Wrong resource
        assert!(
            validate_enum_namespace(
                "aws.s.VersioningStatus.Enabled",
                &legacy_identity("VersioningStatus", "aws.s3")
            )
            .is_err()
        );
        // Wrong type name
        assert!(
            validate_enum_namespace(
                "aws.s3.Versioning.Enabled",
                &legacy_identity("VersioningStatus", "aws.s3")
            )
            .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_5_part_valid() {
        assert!(
            validate_enum_namespace(
                "aws.s3.Bucket.VersioningStatus.Enabled",
                &legacy_identity("VersioningStatus", "aws.s3.Bucket")
            )
            .is_ok()
        );
        assert!(
            validate_enum_namespace(
                "awscc.ec2.Vpc.InstanceTenancy.default",
                &legacy_identity("InstanceTenancy", "awscc.ec2.Vpc")
            )
            .is_ok()
        );
        // 5-part with digit-led tail (`2012_10_17`) — used for IAM policy
        // version identifiers (carina#3051). TypeName at index 3 is preserved.
        assert!(
            validate_enum_namespace(
                "aws.iam.PolicyDocument.Version.2012_10_17",
                &legacy_identity("Version", "aws.iam.PolicyDocument")
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
                &legacy_identity("InstanceTenancy", "awscc.ec2.Vpc")
            )
            .is_err()
        );
        // Wrong type name
        assert!(
            validate_enum_namespace(
                "awscc.ec2.Vpc.Tenancy.default",
                &legacy_identity("InstanceTenancy", "awscc.ec2.Vpc")
            )
            .is_err()
        );
    }

    #[test]
    fn test_validate_namespace_wrong_part_count() {
        // Too many parts for 1-segment namespace
        assert!(
            validate_enum_namespace(
                "foo.bar.baz.ap_northeast_1",
                &legacy_identity("Region", "aws")
            )
            .is_err()
        );
        // 6-part is invalid for 3-segment namespace
        assert!(
            validate_enum_namespace(
                "a.b.c.d.e.f",
                &legacy_identity("VersioningStatus", "aws.s3.Bucket")
            )
            .is_err()
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
                &legacy_identity("Type", "awscc.ec2.vpn_gateway")
            )
            .is_err()
        );
        // Double-namespace patterns are also rejected.
        assert!(
            validate_enum_namespace(
                "awscc.Region.awscc.Region.ap_northeast_1",
                &legacy_identity("Region", "awscc")
            )
            .is_err()
        );
        assert!(
            validate_enum_namespace(
                "aws.Region.aws.Region.us_west_2",
                &legacy_identity("Region", "aws")
            )
            .is_err()
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
            let identity = legacy_identity(type_name, namespace);
            let result = validate_enum_namespace(input, &identity);
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
        use crate::schema::TypeIdentity;
        // (input, identity, expected_output)
        // A bare identity (no provider axis) passes shorthand through;
        // a provider-scoped identity expands bare/2-part inputs into the
        // full `{identity}.{value}` form.
        let cases: &[(Value, TypeIdentity, Value)] = &[
            // bare member + provider-scoped identity → fully qualified
            (
                Value::Concrete(ConcreteValue::String("dedicated".into())),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // bare member, bare identity → passthrough
            (
                Value::Concrete(ConcreteValue::String("dedicated".into())),
                TypeIdentity::bare("InstanceTenancy"),
                Value::Concrete(ConcreteValue::String("dedicated".into())),
            ),
            // TypeName.member matching the enum type name → fully qualified
            (
                Value::Concrete(ConcreteValue::String("InstanceTenancy.dedicated".into())),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // TypeName.member with foreign type name → passthrough
            (
                Value::Concrete(ConcreteValue::String("Tenancy.dedicated".into())),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String("Tenancy.dedicated".into())),
            ),
            // Already-fully-qualified → passthrough (parser returns
            // `FullyQualified`, helper only acts on `TypeQualified`)
            (
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // Lowercase first segment → not a TypeName; passthrough
            (
                Value::Concrete(ConcreteValue::String("instanceTenancy.dedicated".into())),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String("instanceTenancy.dedicated".into())),
            ),
            // Non-string → passthrough
            (
                Value::Concrete(ConcreteValue::Bool(true)),
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Whatever"),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
            // Provider-scoped identity with `segments` (typical S2.5 form
            // — `aws.AvailabilityZone.ZoneName.us_east_1a`): bare value
            // expands into the full dotted form including the kind.
            (
                Value::Concrete(ConcreteValue::String("us_east_1a".into())),
                TypeIdentity::new(Some("aws"), ["AvailabilityZone"], "ZoneName"),
                Value::Concrete(ConcreteValue::String(
                    "aws.AvailabilityZone.ZoneName.us_east_1a".into(),
                )),
            ),
        ];
        for (input, identity, expected) in cases {
            let actual = expand_enum_shorthand(input, identity);
            assert_eq!(actual, *expected, "input={input:?} identity={identity:?}",);
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
                identity: Some(crate::schema::string_enum_identity(
                    "VersioningStatus",
                    Some("aws.s3.Bucket"),
                )),
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
                identity: Some(crate::schema::string_enum_identity(
                    "Effect",
                    Some("aws.iam.PolicyDocument"),
                )),
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
                identity: Some(crate::schema::string_enum_identity(
                    "VersioningStatus",
                    Some("aws.s3.Bucket"),
                )),
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

    /// awscc#251: pin the wiring-level helper, not just the leaf walker.
    /// Proves registry resolution + per-`ResourceId` keying work end to
    /// end — the part the isolated `ResourceSchema::validate` test in
    /// schema/tests.rs does not exercise. A resource whose schema is in
    /// the registry gets its loaded `String` state lifted; a resource
    /// absent from the registry is skipped without panic.
    #[test]
    fn lift_saved_state_string_enums_resolves_schema_per_resource() {
        use crate::resource::{ConcreteValue, Resource, ResourceId, Value};
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
        };
        use std::collections::HashMap;

        let version_enum = AttributeType::StringEnum {
            name: "Version".to_string(),
            values: vec!["2012-10-17".to_string()],
            identity: Some(crate::schema::string_enum_identity(
                "Version",
                Some("aws.iam.PolicyDocument"),
            )),
            dsl_aliases: vec![("2012-10-17".to_string(), "2012_10_17".to_string())],
        };
        let policy_struct = AttributeType::Struct {
            name: "PolicyDocument".to_string(),
            fields: vec![StructField::new("version", version_enum)],
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            ResourceSchema::new("iam.RolePolicy")
                .attribute(AttributeSchema::new("policy_document", policy_struct)),
        );

        let known = Resource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        let unknown = Resource::with_provider("awscc", "iam.Unknown", "x", None);

        let mut pd = indexmap::IndexMap::new();
        pd.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012-10-17".to_string())),
        );
        let mut known_attrs = HashMap::new();
        known_attrs.insert(
            "policy_document".to_string(),
            Value::Concrete(ConcreteValue::Map(pd)),
        );
        let mut unknown_attrs = HashMap::new();
        unknown_attrs.insert(
            "whatever".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );

        let mut saved: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        saved.insert(known.id.clone(), known_attrs);
        saved.insert(unknown.id.clone(), unknown_attrs);

        lift_saved_state_string_enums(&mut saved, &[known.clone(), unknown.clone()], &registry);

        let Value::Concrete(ConcreteValue::Map(pd)) = &saved[&known.id]["policy_document"] else {
            panic!("policy_document map");
        };
        assert_eq!(
            pd["version"],
            Value::Concrete(ConcreteValue::EnumIdentifier("2012_10_17".to_string())),
            "resource present in registry must have its state lifted"
        );
        // Schema-less resource: untouched, no panic.
        assert_eq!(
            saved[&unknown.id]["whatever"],
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
            "resource absent from registry is skipped unchanged"
        );
    }

    /// awscc#251 follow-up: the read-back map (`current_states`) must be
    /// lifted too — #3055 only covered `saved_attrs`, so a refresh whose
    /// `provider.read()` returns plain-String IAM enum values still
    /// failed the strict validator.
    #[test]
    fn lift_current_state_string_enums_lifts_provider_read_state() {
        use crate::resource::{ConcreteValue, Resource, ResourceId, State, Value};
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
        };
        use std::collections::HashMap;

        let version_enum = AttributeType::StringEnum {
            name: "Version".to_string(),
            values: vec!["2012-10-17".to_string()],
            identity: Some(crate::schema::string_enum_identity(
                "Version",
                Some("aws.iam.PolicyDocument"),
            )),
            dsl_aliases: vec![("2012-10-17".to_string(), "2012_10_17".to_string())],
        };
        let policy_struct = AttributeType::Struct {
            name: "PolicyDocument".to_string(),
            fields: vec![StructField::new("version", version_enum)],
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            ResourceSchema::new("iam.Role").attribute(AttributeSchema::new(
                "assume_role_policy_document",
                policy_struct,
            )),
        );

        let role = Resource::with_provider("awscc", "iam.Role", "bs.bootstrap.role", None);

        let mut pd = indexmap::IndexMap::new();
        pd.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012-10-17".to_string())),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "assume_role_policy_document".to_string(),
            Value::Concrete(ConcreteValue::Map(pd)),
        );

        let mut current: HashMap<ResourceId, State> = HashMap::new();
        current.insert(role.id.clone(), State::existing(role.id.clone(), attrs));

        lift_current_state_string_enums(&mut current, std::slice::from_ref(&role), &registry);

        let Value::Concrete(ConcreteValue::Map(pd)) =
            &current[&role.id].attributes["assume_role_policy_document"]
        else {
            panic!("assume_role_policy_document map");
        };
        assert_eq!(
            pd["version"],
            Value::Concrete(ConcreteValue::EnumIdentifier("2012_10_17".to_string())),
            "provider-read state String must be lifted to EnumIdentifier"
        );
    }
}
