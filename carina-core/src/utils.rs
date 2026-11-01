//! Shared utility functions for value normalization and conversion

use crate::resource::{ConcreteValue, Value};
use crate::schema::AttributeType;
use std::path::{Component, Path, PathBuf};

/// Lexically normalize a diagnostic source path without consulting the
/// filesystem. Parent components cancel an immediately preceding normal
/// component, while leading parents remain intact.
pub fn normalize_source_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) =>
            {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

pub(crate) fn truncate_chars_for_display(value: &str, max_chars: usize) -> (String, &'static str) {
    let mut chars = value.chars();
    let rendered = chars.by_ref().take(max_chars).collect();
    let marker = if chars.next().is_some() { "…" } else { "" };
    (rendered, marker)
}

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
/// or digits; mirrors the legacy namespaced-text rule.
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
/// Expands the typed [`ConcreteValue::EnumIdentifier`] shapes the DSL allows
/// for `Enum` and enum-like `Custom` attributes:
///
/// - bare member (`dedicated`) → `{identity}.dedicated`
/// - `TypeName.member` shorthand (`InstanceTenancy.dedicated`) →
///   `{identity}.dedicated`, only when the type name matches the
///   identity's enum type name
/// - any other identifier (already-qualified, foreign type name, identity
///   with no provider axis) → returned unchanged as text
///
/// Values of every other concrete type, especially quoted
/// [`ConcreteValue::String`] values, are returned unchanged.
///
/// Used by both `AttributeType::resolve_value` and the LSP diagnostic
/// pipeline so the two paths cannot drift.
pub fn expand_enum_shorthand(value: &Value, identity: &crate::schema::TypeIdentity) -> Value {
    // Only the parser's typed `EnumIdentifier` form is eligible for namespace
    // expansion. Quoted strings remain literal even when their text resembles
    // `TypeName.member`.
    //
    // The expanded form is the dotted display of the structured
    // identity followed by the value: `{provider}.{segments...}.{kind}.{value}`
    // — same shape as the type's `TypeIdentity::Display`. For a bare
    // identity with no provider axis the shorthand passes through
    // unchanged (no namespace to prefix).
    let Value::Concrete(ConcreteValue::EnumIdentifier(identifier)) = value else {
        return value.clone();
    };
    // The kind is the type's own name and matches the leading
    // `TypeName` of the 2-part `TypeName.value` shorthand:
    // `InstanceTenancy.dedicated` against a Custom whose kind is
    // `InstanceTenancy`, or `ZoneName.us_east_1a` against the
    // zone-name AvailabilityZone type.
    let enum_type_name: &str = &identity.kind;
    let text = match identifier.as_str() {
        s if !s.contains('.') && identity.provider.is_some() => {
            format!("{identity}.{s}")
        }
        s if !s.contains('.') => s.to_string(),
        s => {
            if let Some(NamespacedId::TypeQualified {
                type_name: ident,
                value: member,
            }) = NamespacedId::parse(s)
                && identity.provider.is_some()
                && ident == enum_type_name
            {
                format!("{identity}.{member}")
            } else {
                s.to_string()
            }
        }
    };
    Value::Concrete(ConcreteValue::String(text.into()))
}

/// A `TypeName` segment — first char uppercase ASCII.
fn is_type_name_segment(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Extract the last dot-separated part from a namespaced identifier.
/// Returns the original string if no dots are present.
///
/// This is a legacy read-normalization helper, not a general-purpose enum
/// wire-value extractor. It mis-splits enum values that themselves contain
/// dots, such as `awscc.ec2.vpn_gateway.Type.ipsec.1` -> `1`; use
/// `extract_enum_value_with_values` internally when valid enum values are
/// available. It must not be used for provider wire-out paths. This helper is
/// crate-internal and is used only as the no-match fallback of
/// `extract_enum_value_with_values`.
pub(crate) fn extract_enum_value(s: &str) -> &str {
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
pub(crate) fn extract_enum_value_with_values<'a>(s: &'a str, valid_values: &[&str]) -> &'a str {
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

    if is_two_part {
        if let Some(id) = NamespacedId::parse(s)
            && id.matches_identity(identity)
        {
            return Ok(());
        }
    } else if is_full_form {
        let Some(value) = s.strip_prefix(&format!("{prefix}.")) else {
            return Err(format!("expected format {}.value", prefix));
        };
        if !value.is_empty() && !value.contains('.') {
            return Ok(());
        }
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

fn shape_for_projection<'a>(
    attr_type: &'a AttributeType,
    defs: Option<&'a std::collections::BTreeMap<String, AttributeType>>,
) -> Option<crate::schema::Shape<'a>> {
    match defs {
        Some(defs) => Some(attr_type.shape_with_defs(defs)),
        None => attr_type.shape_ref_free().ok(),
    }
}

/// Lift every `ConcreteValue::String` that sits at a `Enum`-typed
/// position to `ConcreteValue::EnumIdentifier`, descending into struct
/// fields, list elements, and map values, when (and only when) the
/// string is a recognized member of that enum.
///
/// # Why this exists (awscc#251)
///
/// carina#2986 Phase 4 made the schema validator *strict*: a
/// `Enum` position only accepts `ConcreteValue::EnumIdentifier`,
/// never `ConcreteValue::String` (a bare quoted string at an enum
/// position is a form error — see the `StringLiteralExpectedEnum`
/// diagnostic). awscc#250 then promoted IAM policy `version`/`effect`
/// from `Custom` to `Enum`.
///
/// Persisted state files (S3/local JSON) written *before* that schema
/// change store these values as plain JSON strings. On load the
/// schema-blind [`crate::value::PersistedValue`] decoder turns ordinary strings
/// into `ConcreteValue::String`. `carina validate`/`plan` then
/// re-validate upstream-state-referenced resources against the *new*
/// schema, and the loaded `String` is rejected at the now-`Enum`
/// position even though the stored value is perfectly valid.
///
/// This function is intentionally schema-aware and general: it does not name
/// IAM, `version`, or `effect` anywhere, so any current or future
/// `Custom`→`Enum` migration is covered without further changes. It is *not* a
/// validator carve-out — the validator stays strict; old state is migrated in
/// memory before validation.
///
/// Only recognized members are lifted. A string is recognized when it
/// equals one of the enum's API-canonical `values` or appears on either
/// side of a `dsl_aliases` pair. Unrecognized strings are left as
/// `ConcreteValue::String` so the strict validator still rejects
/// genuinely-invalid state instead of silently masking it.
///
/// Recognized leaves become `CanonicalEnum`, retaining both the schema type
/// identity and provider API spelling. A plain `String` is matched literally
/// against schema values or aliases; its dotted text is never treated as a
/// namespace. A typed `EnumIdentifier` may use namespace extraction and DSL
/// alias mapping because its variant already carries enum intent.
///
/// `Union` types are not recursed into because the active arm cannot be known
/// without re-running validation.
pub fn lift_state_enum_leaves(
    attributes: &mut std::collections::HashMap<String, Value>,
    schema: &crate::schema::ResourceSchema,
) {
    for (name, attr) in &schema.attributes {
        if let Some(value) = attributes.get(name)
            && let Some(lifted) = lift_enum_leaves_with_defs(value, &attr.attr_type, &schema.defs)
        {
            attributes.insert(name.clone(), lifted);
        }
    }
}

/// Internal saved-state reconciliation worker for
/// [`RawSavedAttrs::lift`](crate::provider::RawSavedAttrs::lift).
///
/// Reconciliation is map-driven: every persisted entry resolves its schema
/// once from its own resource ID, then receives both the enum lift and
/// structural canonicalization. Entries whose schema is not in `registry` are
/// skipped.
pub(crate) fn lift_saved_state_enum_leaves(
    saved_attrs: &mut std::collections::HashMap<
        crate::resource::ResourceId,
        std::collections::HashMap<String, Value>,
    >,
    registry: &crate::schema::SchemaRegistry,
) {
    // carina#3739: hydration is map-driven, so reconciliation must cover the
    // same map. This includes materialized for-loop children before an expanded
    // resource slice is available.
    for (id, attrs) in saved_attrs.iter_mut() {
        if let Some(schema) = registry.get(
            &id.provider,
            &id.resource_type,
            crate::schema::SchemaKind::Resource,
        ) {
            lift_state_enum_leaves(attrs, schema.as_schema());
            crate::value::canonicalize_state_attribute_map_with_schema(attrs, schema.schema());
        }
    }
}

/// Apply [`lift_state_enum_leaves`] to every managed resource's
/// **read-back** state attributes, resolving each resource's schema from
/// `registry`.
///
/// [`lift_saved_state_enum_leaves`] only migrates the cached
/// `saved_attrs` map (state-file JSON). On a refresh, the live value is
/// produced by `provider.read()` and lands in managed current state, a
/// *different* map the saved-attrs lift never touches. A provider that
/// returns an IAM policy document with plain `String` `version` /
/// `effect` (the on-the-wire shape for a field that was `Custom` when
/// the resource was created, now `Enum` after awscc#250) then
/// flows un-lifted into the differ, where the strict carina#2986
/// validator rejects it — the exact failure awscc#251's first cut
/// (#3055) did not fix because it only covered `saved_attrs`. Call this
/// once after both refresh branches have populated managed current state,
/// before the differ / resolver consume it.
///
/// Resources whose schema is not in `registry` (or that have no state)
/// are skipped.
pub fn lift_current_state_enum_leaves<S, D>(
    current_states: &mut std::collections::HashMap<
        crate::resource::ResourceId,
        crate::resource::Current,
    >,
    resources: &[crate::resource::Resource<S, D>],
    registry: &crate::schema::SchemaRegistry,
) {
    for resource in resources {
        if let Some(schema) = registry.get_for(resource)
            && let Some(state) = current_states.get_mut(&resource.id)
            && let Some(attributes) = state.attributes_mut()
        {
            lift_state_enum_leaves(attributes, schema.as_schema());
        }
    }
}

/// [`DataSource`](crate::resource::DataSource) counterpart of
/// [`lift_current_state_enum_leaves`]. Schema lookup routes through the
/// data-source registry (`get_for_data_source`) so a `read` resource's
/// provider-returned state is Enum-lifted too (carina#3181).
pub fn lift_current_state_enum_leaves_for_data_sources(
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
            lift_state_enum_leaves(&mut state.attributes, schema.as_schema());
        }
    }
}

/// Value-level worker for [`lift_state_enum_leaves`].
///
/// Returns `Some(new_value)` when at least one nested value was lifted and
/// `None` when nothing changed, so callers can skip cloning.
pub fn lift_enum_leaves(value: &Value, attr_type: &AttributeType) -> Option<Value> {
    lift_enum_leaves_projected(value, attr_type, None)
}

/// Same as [`lift_enum_leaves`] but takes the enclosing
/// [`ResourceSchema::defs`] map so cyclic CFN definitions
/// (`AttributeType::Ref`) are followed during the value walk
/// (carina#3340). Callers that hold a resource schema should prefer
/// this entry point.
pub fn lift_enum_leaves_with_defs(
    value: &Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Option<Value> {
    lift_enum_leaves_projected(value, attr_type, Some(defs))
}

fn lift_enum_leaves_projected(
    value: &Value,
    attr_type: &AttributeType,
    defs: Option<&std::collections::BTreeMap<String, AttributeType>>,
) -> Option<Value> {
    // Leaf case: this position is itself an Enum.
    if let Some((identity, values, _, validate, dsl_map)) = attr_type.enum_parts() {
        let raw = match value {
            Value::Concrete(ConcreteValue::String(s)) => {
                Some((s.expose_plaintext(), EnumLeafSource::String))
            }
            Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
                Some((s.as_str(), EnumLeafSource::EnumIdentifier))
            }
            _ => None,
        };
        if let Some((s, source)) = raw
            && enum_member_is_recognized_for_state_lift(
                identity, values, validate, dsl_map, s, source,
            )
            && let Ok(canonical) = match (defs, source) {
                (Some(defs), EnumLeafSource::String) => {
                    crate::resource::EnumValueResolver::with_defs(attr_type, defs)
                        .resolve_state_text(s)
                }
                (None, EnumLeafSource::String) => {
                    crate::resource::EnumValueResolver::new(attr_type).resolve_state_text(s)
                }
                (Some(defs), EnumLeafSource::EnumIdentifier) => {
                    crate::resource::EnumValueResolver::with_defs(attr_type, defs)
                        .resolve_raw(&crate::resource::RawEnumIdentifier::parse(s))
                }
                (None, EnumLeafSource::EnumIdentifier) => {
                    crate::resource::EnumValueResolver::new(attr_type)
                        .resolve_raw(&crate::resource::RawEnumIdentifier::parse(s))
                }
            }
        {
            return Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)));
        }
        // Recognized-or-not, an Enum leaf has no children.
        return None;
    }

    // Project onto Shape so `Ref` is either peeled through schema defs
    // or rejected by the bare ref-free path. The wildcard arm cannot
    // silently swallow a `Ref` because `Shape` has no `Ref` variant.
    match shape_for_projection(attr_type, defs)? {
        crate::schema::Shape::Struct { .. } => {
            let fields = match defs {
                Some(defs) => crate::schema::struct_fields_with_defs(attr_type, defs)?,
                None => match attr_type.raw_shape() {
                    crate::schema::RawShape::Struct { fields, .. } => fields,
                    _ => return None,
                },
            };
            let Value::Concrete(ConcreteValue::Map(map)) = value else {
                return None;
            };
            let mut rewritten = map.clone();
            let mut changed = false;
            for field in fields {
                if let Some(field_value) = map.get(&field.name)
                    && let Some(new_field) =
                        lift_enum_leaves_projected(field_value, &field.field_type, defs)
                {
                    rewritten.insert(field.name.clone(), new_field);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        crate::schema::Shape::List {
            element_type: inner,
            ..
        } => {
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return None;
            };
            let mut rewritten = items.clone();
            let mut changed = false;
            for (i, item) in items.iter().enumerate() {
                if let Some(new_item) = lift_enum_leaves_projected(item, inner, defs) {
                    rewritten[i] = new_item;
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::List(rewritten)))
        }
        crate::schema::Shape::Map { value: inner, .. } => {
            let Value::Concrete(ConcreteValue::Map(map)) = value else {
                return None;
            };
            let mut rewritten = map.clone();
            let mut changed = false;
            for (k, v) in map {
                if let Some(new_v) = lift_enum_leaves_projected(v, inner, defs) {
                    rewritten.insert(k.clone(), new_v);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        // Scalars and Union: nothing to descend into. `Ref` is
        // already peeled by `shape(defs)` above.
        _ => None,
    }
}

fn enum_member_is_recognized_for_state_lift(
    identity: &crate::schema::TypeIdentity,
    values: Option<&[String]>,
    validate: Option<&crate::schema::CustomValidator>,
    dsl_map: crate::schema::DslMap<'_>,
    value: &str,
    source: EnumLeafSource,
) -> bool {
    let valid: Vec<&str> = values.into_iter().flatten().map(String::as_str).collect();
    let trailing = match source {
        EnumLeafSource::String => value,
        EnumLeafSource::EnumIdentifier => extract_enum_value_with_values(value, &valid),
    };
    let api = dsl_map.api_for(trailing);
    let dsl = dsl_map.dsl_for(trailing);
    if values.into_iter().flatten().any(|candidate| {
        candidate == value || candidate == trailing || candidate == &api || dsl == *candidate
    }) {
        return true;
    }
    if values.is_some() {
        return false;
    }
    if let Some(validate) = validate {
        return validate(&Value::Concrete(ConcreteValue::String(api.into()))).is_ok();
    }
    dynamic_enum_member_is_structural(identity, &dsl, value, source)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnumLeafSource {
    String,
    EnumIdentifier,
}

fn dynamic_enum_member_is_structural(
    identity: &crate::schema::TypeIdentity,
    dsl: &str,
    original: &str,
    source: EnumLeafSource,
) -> bool {
    if source == EnumLeafSource::EnumIdentifier
        && NamespacedId::parse(original).is_some_and(|id| id.matches_identity(identity))
    {
        return true;
    }
    let full = identity
        .dotted_prefix()
        .map(|prefix| format!("{}.{}.{}", prefix, identity.kind, dsl))
        .unwrap_or_else(|| format!("{}.{}", identity.kind, dsl));
    if NamespacedId::parse(&full).is_none() || !is_dynamic_dsl_member_segment(dsl) {
        return false;
    }
    match source {
        EnumLeafSource::EnumIdentifier => true,
        EnumLeafSource::String => dsl != original && dsl.chars().any(|c| c.is_ascii_digit()),
    }
}

fn is_dynamic_dsl_member_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.chars().next().is_some_and(|c| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

/// Read a provider-ready config's `region` attribute, falling back to
/// `default_region` when it is absent or unresolved.
///
/// Enum-typed configs reach this helper as [`ConcreteValue::CanonicalEnum`]
/// after host schema resolution. A [`ConcreteValue::String`] is always a
/// literal and is returned unchanged; this helper never strips or folds text
/// that merely resembles DSL enum syntax.
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
/// let empty: IndexMap<String, Value> = IndexMap::new();
/// assert_eq!(extract_region_from_attrs(&empty, "ap-northeast-1"), "ap-northeast-1");
/// ```
pub fn extract_region_from_attrs(
    attributes: &indexmap::IndexMap<String, crate::resource::Value>,
    default_region: &str,
) -> String {
    use crate::resource::{ConcreteValue, Value};
    match attributes.get("region") {
        Some(Value::Concrete(ConcreteValue::String(s))) => s.expose_plaintext().to_string(),
        Some(Value::Concrete(ConcreteValue::CanonicalEnum(value))) => value.api_value().to_string(),
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
    use crate::schema::{TypeIdentity, enum_identity};

    #[test]
    fn normalize_source_path_collapses_parent_components_lexically() {
        assert_eq!(
            normalize_source_path(Path::new("mod/../inner/main.crn")),
            PathBuf::from("inner/main.crn")
        );
        assert_eq!(
            normalize_source_path(Path::new("../mod/../inner/main.crn")),
            PathBuf::from("../inner/main.crn")
        );
    }

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
    fn truncate_chars_for_display_is_unicode_safe() {
        assert_eq!(
            truncate_chars_for_display("abあいう", 4),
            ("abあい".to_string(), "…")
        );
        assert_eq!(
            truncate_chars_for_display("abあい", 4),
            ("abあい".to_string(), "")
        );
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
    fn test_validate_namespace_deep_structural_full_path_valid() {
        let identity = enum_identity("Status", Some("aws.s3.BucketLifecycleConfiguration.Rules"));

        assert!(
            validate_enum_namespace(
                "aws.s3.BucketLifecycleConfiguration.Rules.Status.enabled",
                &identity,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_namespace_deep_structural_wrong_middle_segment_invalid() {
        let identity = enum_identity("Status", Some("aws.s3.BucketLifecycleConfiguration.Rules"));

        assert!(
            validate_enum_namespace("aws.s3.WrongStruct.Rules.Status.enabled", &identity).is_err()
        );
    }

    #[test]
    fn test_validate_namespace_deep_structural_dotted_value_invalid() {
        let identity = enum_identity("Type", Some("aws.x.Y.Z"));

        assert!(validate_enum_namespace("aws.x.Y.Z.Type.ipsec.1", &identity).is_err());
    }

    #[test]
    fn test_validate_namespace_deep_structural_wrong_identity_invalid() {
        let identity = enum_identity("Status", Some("aws.ec2.SecurityGroup.Rules"));

        assert!(
            validate_enum_namespace(
                "aws.s3.BucketLifecycleConfiguration.Rules.Status.enabled",
                &identity,
            )
            .is_err()
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
    fn test_extract_enum_value_with_values_deep_structural_plain_value() {
        let values = &["enabled", "disabled"];
        assert_eq!(
            extract_enum_value_with_values(
                "aws.s3.BucketLifecycleConfiguration.Rules.Status.enabled",
                values,
            ),
            "enabled"
        );
    }

    #[test]
    fn test_extract_enum_value_with_values_deep_structural_dotted_value() {
        let values = &["ipsec.1"];
        assert_eq!(
            extract_enum_value_with_values("aws.x.Y.Z.Type.ipsec.1", values),
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
                Value::Concrete(ConcreteValue::enum_identifier("dedicated")),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // bare member, bare identity → passthrough
            (
                Value::Concrete(ConcreteValue::enum_identifier("dedicated")),
                crate::schema::TypeIdentity::bare("InstanceTenancy"),
                Value::Concrete(ConcreteValue::String("dedicated".into())),
            ),
            // TypeName.member matching the enum type name → fully qualified
            (
                Value::Concrete(ConcreteValue::enum_identifier("InstanceTenancy.dedicated")),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // TypeName.member with foreign type name → passthrough
            (
                Value::Concrete(ConcreteValue::enum_identifier("Tenancy.dedicated")),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String("Tenancy.dedicated".into())),
            ),
            // Already-fully-qualified → passthrough (parser returns
            // `FullyQualified`, helper only acts on `TypeQualified`)
            (
                Value::Concrete(ConcreteValue::enum_identifier(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated",
                )),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String(
                    "awscc.ec2.Vpc.InstanceTenancy.dedicated".into(),
                )),
            ),
            // Lowercase first segment → not a TypeName; passthrough
            (
                Value::Concrete(ConcreteValue::enum_identifier("instanceTenancy.dedicated")),
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
                Value::Concrete(ConcreteValue::enum_identifier("us_east_1a")),
                TypeIdentity::new(Some("aws"), ["AvailabilityZone"], "ZoneName"),
                Value::Concrete(ConcreteValue::String(
                    "aws.AvailabilityZone.ZoneName.us_east_1a".into(),
                )),
            ),
            // Quoted text remains text even when its shape resembles a typed
            // enum shorthand.
            (
                Value::Concrete(ConcreteValue::String("InstanceTenancy.dedicated".into())),
                TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
                Value::Concrete(ConcreteValue::String("InstanceTenancy.dedicated".into())),
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
        fn unresolved_enum_identifier_uses_default() {
            let attrs = attrs_with_region(Value::Concrete(ConcreteValue::enum_identifier(
                "aws.Region.us_east_1".to_string(),
            )));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "ap-northeast-1"
            );
        }

        #[test]
        fn canonical_enum_uses_api_spelling() {
            let attrs = attrs_with_region(Value::Concrete(ConcreteValue::CanonicalEnum(
                crate::resource::CanonicalEnumValue::new_for_test(
                    crate::schema::enum_identity("Region", Some("aws")),
                    "us-east-1",
                ),
            )));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "us-east-1"
            );
        }

        #[test]
        fn string_aws_sdk_form() {
            let attrs =
                attrs_with_region(Value::Concrete(ConcreteValue::String("us-east-1".into())));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "us-east-1"
            );
        }

        #[test]
        fn string_namespaced_form() {
            // A quoted string is a literal, even when its text resembles
            // namespaced DSL enum syntax.
            let attrs = attrs_with_region(Value::Concrete(ConcreteValue::String(
                "aws.Region.us_east_1".into(),
            )));
            assert_eq!(
                extract_region_from_attrs(&attrs, "ap-northeast-1"),
                "aws.Region.us_east_1"
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
    fn lift_saved_state_enum_leaves_resolves_schema_per_resource() {
        use crate::resource::{ConcreteValue, Resource, ResourceId, Value};
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
        };
        use std::collections::HashMap;

        let version_enum = AttributeType::enum_(
            crate::schema::enum_identity("Version", Some("aws.iam.PolicyDocument")),
            Some(vec!["2012-10-17".to_string()]),
            vec![("2012-10-17".to_string(), "2012_10_17".to_string())],
            None,
            None,
        );
        let policy_struct = AttributeType::struct_(
            "PolicyDocument".to_string(),
            vec![StructField::optional("version", version_enum).build()],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            (ResourceSchema::new("iam.RolePolicy")
                .attribute(AttributeSchema::optional("policy_document", policy_struct).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let known = Resource::with_provider("awscc", "iam.RolePolicy", "rp", None);
        let unknown = Resource::with_provider("awscc", "iam.Unknown", "x", None);

        let mut pd = indexmap::IndexMap::new();
        pd.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012-10-17".into())),
        );
        let mut known_attrs = HashMap::new();
        known_attrs.insert(
            "policy_document".to_string(),
            Value::Concrete(ConcreteValue::Map(pd)),
        );
        let mut unknown_attrs = HashMap::new();
        unknown_attrs.insert(
            "whatever".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        );

        let mut saved: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        saved.insert(known.id.clone(), known_attrs);
        saved.insert(unknown.id.clone(), unknown_attrs);

        lift_saved_state_enum_leaves(&mut saved, &registry);

        let Value::Concrete(ConcreteValue::Map(pd)) = &saved[&known.id]["policy_document"] else {
            panic!("policy_document map");
        };
        assert!(
            matches!(
                &pd["version"],
                Value::Concrete(ConcreteValue::CanonicalEnum(c))
                    if c.identity().to_string() == "aws.iam.PolicyDocument.Version"
                        && c.api_value() == "2012-10-17"
            ),
            "resource present in registry must have its state lifted"
        );
        // Schema-less resource: untouched, no panic.
        assert_eq!(
            saved[&unknown.id]["whatever"],
            Value::Concrete(ConcreteValue::String("Allow".into())),
            "resource absent from registry is skipped unchanged"
        );
    }

    /// awscc#251 follow-up: the managed read-back map must be
    /// lifted too — #3055 only covered `saved_attrs`, so a refresh whose
    /// `provider.read()` returns plain-String IAM enum values still
    /// failed the strict validator.
    #[test]
    fn lift_current_state_enum_leaves_lifts_provider_read_state() {
        use crate::resource::{ConcreteValue, Current, Resource, ResourceId, State, Value};
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
        };
        use std::collections::HashMap;

        let version_enum = AttributeType::enum_(
            crate::schema::enum_identity("Version", Some("aws.iam.PolicyDocument")),
            Some(vec!["2012-10-17".to_string()]),
            vec![("2012-10-17".to_string(), "2012_10_17".to_string())],
            None,
            None,
        );
        let policy_struct = AttributeType::struct_(
            "PolicyDocument".to_string(),
            vec![StructField::optional("version", version_enum).build()],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            (ResourceSchema::new("iam.Role").attribute(
                AttributeSchema::optional("assume_role_policy_document", policy_struct).build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let role = Resource::with_provider("awscc", "iam.Role", "bs.bootstrap.role", None);

        let mut pd = indexmap::IndexMap::new();
        pd.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012-10-17".into())),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "assume_role_policy_document".to_string(),
            Value::Concrete(ConcreteValue::Map(pd)),
        );

        let mut current: HashMap<ResourceId, Current> = HashMap::new();
        current.insert(
            role.id.clone(),
            Current::from_state(State::existing(role.id.clone(), attrs)),
        );

        lift_current_state_enum_leaves(&mut current, std::slice::from_ref(&role), &registry);

        let state = match &current[&role.id] {
            Current::Absent(state) => state.state(),
            Current::Present(present) => present.state(),
            Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
            Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            Current::MissingIdentifier(state) => state.state(),
        };
        let Value::Concrete(ConcreteValue::Map(pd)) =
            &state.attributes["assume_role_policy_document"]
        else {
            panic!("assume_role_policy_document map");
        };
        assert!(
            matches!(
                &pd["version"],
                Value::Concrete(ConcreteValue::CanonicalEnum(c))
                    if c.identity().to_string() == "aws.iam.PolicyDocument.Version"
                        && c.api_value() == "2012-10-17"
            ),
            "provider-read state String must be lifted to CanonicalEnum"
        );
    }

    #[test]
    fn lift_enum_leaves_does_not_strip_a_dotted_string_prefix() {
        let attr_type = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Format"),
            Some(vec!["json".to_string()]),
            vec![],
            None,
            None,
        );
        let quoted = Value::Concrete(ConcreteValue::String("A.json".into()));

        assert_eq!(lift_enum_leaves(&quoted, &attr_type), None);
    }
}
