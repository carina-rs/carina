//! Identifier and prefix handling for resources
//!
//! Functions for generating random suffixes, resolving attribute prefixes,
//! reconciling prefixed names with state, and computing anonymous resource identifiers.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::parser::ProviderConfig;
use crate::resource::{ConcreteValue, DeferredValue, Resource, ResourceId, Value};
use crate::schema::{AttributeType, ResourceSchema, SchemaRegistry};
use crate::utils::{extract_enum_value_with_values, validate_enum_namespace};
use crate::validation::is_string_compatible_type;

/// Generate a random 8-character lowercase hex suffix using UUID v4.
pub fn generate_random_suffix() -> String {
    let uuid = uuid::Uuid::new_v4();
    let hex = uuid.as_simple().to_string();
    hex[..8].to_string()
}

/// Resolve `<attr>_prefix` meta-attributes in resources.
///
/// For each resource attribute ending in `_prefix`, checks if the base attribute
/// (without `_prefix`) exists in the schema as a string-compatible type. If so:
/// - Removes the `_prefix` attribute
/// - Stores the prefix in `resource.prefixes`
/// - Generates a temporary name: `prefix + random_suffix`
///
/// Errors if both `<attr>_prefix` and `<attr>` are specified, or if prefix is empty.
pub fn resolve_attr_prefixes(
    resources: &mut [Resource],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources.iter_mut() {
        let schema = match registry.get_for(resource) {
            Some(s) => s,
            None => continue, // Unknown resource type; validate_resources will catch this
        };

        // Collect prefix attributes to process
        let prefix_attrs: Vec<(String, String)> = resource
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                if let Some(base_attr) = key.strip_suffix("_prefix")
                    && let Value::Concrete(ConcreteValue::String(prefix_value)) = value
                    && let Some(attr_schema) = schema.attributes.get(base_attr)
                    && is_string_compatible_type(&attr_schema.attr_type, &schema.defs)
                {
                    return Some((base_attr.to_string(), prefix_value.clone()));
                }
                None
            })
            .collect();

        for (base_attr, prefix_value) in prefix_attrs {
            let prefix_key = format!("{}_prefix", base_attr);

            // Error if prefix is empty
            if prefix_value.is_empty() {
                all_errors.push(format!("{}: '{}' cannot be empty", resource.id, prefix_key));
                continue;
            }

            // Error if both prefix and base attribute are specified
            if resource.attributes.contains_key(&base_attr) {
                all_errors.push(format!(
                    "{}: cannot specify both '{}' and '{}'",
                    resource.id, prefix_key, base_attr
                ));
                continue;
            }

            // Remove the _prefix attribute. `shift_remove` (not
            // `swap_remove`) preserves the source-order of the rest of
            // the attributes, which is the whole point of #2222.
            resource.attributes.shift_remove(&prefix_key);

            // Store prefix
            resource
                .prefixes
                .insert(base_attr.clone(), prefix_value.clone());

            // Generate temporary name
            let generated_name = format!("{}{}", prefix_value, generate_random_suffix());
            resource.set_attr(
                base_attr,
                Value::Concrete(ConcreteValue::String(generated_name)),
            );
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// State information needed for prefix reconciliation.
pub struct PrefixStateInfo {
    /// Attribute prefixes stored in state (e.g., {"bucket_name": "my-app-"})
    pub prefixes: HashMap<String, String>,
    /// Attribute string values from state (e.g., {"bucket_name": "my-app-existing1"})
    pub attribute_values: HashMap<String, String>,
}

/// Reconcile prefixed names with existing state.
///
/// For resources that have prefixes and a matching entry in state (same prefix),
/// reuses the existing name from state instead of the temporarily generated one.
/// If the prefix has changed or there's no state match, keeps the new generated name.
pub fn reconcile_prefixed_names(
    resources: &mut [Resource],
    find_state: &dyn Fn(&str, &str, &str) -> Option<PrefixStateInfo>,
) {
    for resource in resources.iter_mut() {
        if resource.prefixes.is_empty() {
            continue;
        }

        // Find matching resource in state
        let state_info = find_state(
            &resource.id.provider,
            &resource.id.resource_type,
            resource.id.name_str(),
        );
        let state_info = match state_info {
            Some(si) => si,
            None => continue,
        };

        let updates: Vec<_> = resource
            .prefixes
            .iter()
            .filter_map(|(attr_name, prefix)| {
                if let Some(state_prefix) = state_info.prefixes.get(attr_name)
                    && state_prefix == prefix
                {
                    state_info.attribute_values.get(attr_name).map(|name_str| {
                        (
                            attr_name.clone(),
                            Value::Concrete(ConcreteValue::String(name_str.clone())),
                        )
                    })
                } else {
                    None
                }
            })
            .collect();
        for (attr_name, value) in updates {
            resource.set_attr(attr_name, value);
        }
    }
}

/// Produce a deterministic string representation of a Value for hashing.
///
/// Unlike `format!("{:?}", value)`, this ensures Map entries are sorted by key,
/// so the output is consistent across runs (HashMap iteration order is random).
fn deterministic_value_string(value: &Value) -> String {
    match value {
        Value::Concrete(ConcreteValue::String(s)) => format!("String({:?})", s),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => format!("EnumIdentifier({:?})", s),
        Value::Concrete(ConcreteValue::Int(i)) => format!("Int({})", i),
        Value::Concrete(ConcreteValue::Float(f)) => format!("Float({})", f),
        Value::Concrete(ConcreteValue::Bool(b)) => format!("Bool({})", b),
        Value::Concrete(ConcreteValue::Duration(d)) => format!("Duration({})", d.as_secs()),
        Value::Concrete(ConcreteValue::List(items)) => {
            let parts: Vec<String> = items.iter().map(deterministic_value_string).collect();
            format!("List([{}])", parts.join(", "))
        }
        Value::Concrete(ConcreteValue::StringList(items)) => {
            let parts: Vec<String> = items.iter().map(|s| format!("{:?}", s)).collect();
            format!("StringList([{}])", parts.join(", "))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{:?}: {}", k, deterministic_value_string(v)))
                .collect();
            format!("Map({{{}}})", parts.join(", "))
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            format!("ResourceRef({})", path.to_dot_string())
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            format!("BindingRef({})", binding)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let strs: Vec<String> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => format!("Literal({:?})", s),
                    InterpolationPart::Expr(v) => {
                        format!("Expr({})", deterministic_value_string(v))
                    }
                })
                .collect();
            format!("Interpolation([{}])", strs.join(", "))
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            let arg_strs: Vec<String> = args.iter().map(deterministic_value_string).collect();
            format!("FunctionCall({}({}))", name, arg_strs.join(", "))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            format!("Secret({})", deterministic_value_string(inner))
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            use crate::resource::UnknownReason;
            match reason {
                UnknownReason::UpstreamRef { path } => {
                    format!("Unknown(UpstreamRef({}))", path.to_dot_string())
                }
                UnknownReason::UpstreamBareRef { binding } => {
                    format!("Unknown(UpstreamBareRef({}))", binding)
                }
                UnknownReason::ForKey => "Unknown(ForKey)".to_string(),
                UnknownReason::ForIndex => "Unknown(ForIndex)".to_string(),
                UnknownReason::ForValue => "Unknown(ForValue)".to_string(),
                UnknownReason::ForValuePath { path } => {
                    format!("Unknown(ForValuePath({}))", path.to_dot_string())
                }
                UnknownReason::EmptyInterpolation => "Unknown(EmptyInterpolation)".to_string(),
            }
        }
    }
}

fn enum_identifier_segments_match(
    raw: &str,
    attribute_type: &AttributeType,
    allow_dotted_value: bool,
) -> bool {
    let Some((identity, _, _, _, _)) = attribute_type.enum_parts() else {
        return false;
    };
    if !raw.contains('.') {
        return true;
    }

    let parts: Vec<&str> = raw.split('.').collect();
    let Some(kind_idx) = parts.iter().position(|part| *part == identity.kind) else {
        return false;
    };
    if kind_idx + 1 >= parts.len() {
        return false;
    }
    if !allow_dotted_value && parts.len() != kind_idx + 2 {
        return false;
    }

    if !identity.segments.is_empty() {
        let prefix = &parts[..kind_idx];
        if prefix.len() < identity.segments.len() {
            return false;
        }
        let segment_start = prefix.len() - identity.segments.len();
        if !prefix[segment_start..]
            .iter()
            .copied()
            .eq(identity.segments.iter().map(String::as_str))
        {
            return false;
        }
    }

    validate_enum_namespace(raw, identity).is_ok()
        || parts
            .get(kind_idx)
            .is_some_and(|kind| *kind == identity.kind)
}

/// Return the anonymous-hash feature string for a schema-known enum value.
///
/// Only parser-surface `EnumIdentifier` values are canonicalized. All other
/// values deliberately keep the legacy deterministic representation so deferred
/// bindings and non-enum hash inputs do not change.
pub(crate) fn canonical_enum_feature_string(
    value: &Value,
    attribute_type: Option<&AttributeType>,
) -> String {
    let Some(attribute_type) = attribute_type else {
        return deterministic_value_string(value);
    };
    let Value::Concrete(ConcreteValue::EnumIdentifier(raw)) = value else {
        return deterministic_value_string(value);
    };
    let Some((_identity, values, _aliases, _validate, dsl_map)) = attribute_type.enum_parts()
    else {
        return deterministic_value_string(value);
    };
    if !enum_identifier_segments_match(raw, attribute_type, values.is_some()) {
        return deterministic_value_string(value);
    }

    let valid_values: Vec<&str> = values.into_iter().flatten().map(String::as_str).collect();
    let variant = extract_enum_value_with_values(raw, &valid_values);
    let api_value = dsl_map.api_for_hash_feature(variant);
    if let Some(values) = values
        && !values.iter().any(|v| v == &api_value)
    {
        return deterministic_value_string(value);
    }

    format!("EnumApiValue({:?})", api_value)
}

fn canonical_create_only_value_string(
    value: &Value,
    attribute_type: Option<&AttributeType>,
) -> Option<String> {
    match value {
        Value::Concrete(ConcreteValue::String(s)) => {
            Some(canonical_create_only_text_string(s, attribute_type))
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(_)) => {
            Some(canonical_enum_feature_string(value, attribute_type))
        }
        _ => None,
    }
}

fn canonical_create_only_text_string(
    value: &str,
    attribute_type: Option<&AttributeType>,
) -> String {
    if attribute_type.and_then(AttributeType::enum_parts).is_none() {
        return value.to_string();
    }

    let enum_value = Value::Concrete(ConcreteValue::EnumIdentifier(value.to_string()));
    let canonical = canonical_enum_feature_string(&enum_value, attribute_type);
    if canonical == deterministic_value_string(&enum_value) {
        value.to_string()
    } else {
        canonical
    }
}

fn canonical_state_create_only_value_string(
    value: &str,
    attribute_type: Option<&AttributeType>,
) -> String {
    canonical_create_only_text_string(value, attribute_type)
}

/// Maximum Hamming distance (out of 64 bits) for SimHash-based reconciliation.
/// Two identifiers with distance below this threshold are considered the "same resource"
/// with modified attributes.
pub(crate) const SIMHASH_HAMMING_THRESHOLD: u32 = 20;

/// Find the unique candidate closest (by Hamming distance) to `target` SimHash
/// among `candidates`, below `SIMHASH_HAMMING_THRESHOLD`. Returns `None` when
/// no candidate qualifies, or when two or more candidates tie at the minimum
/// distance — the latter is an ambiguous match the caller should refuse to
/// commit to (rebinding to the wrong state entry would silently corrupt
/// addresses).
pub(crate) fn closest_unique_simhash_match<C: Copy>(
    target: u64,
    candidates: impl IntoIterator<Item = C>,
    hash_of: impl Fn(C) -> u64,
) -> Option<C> {
    let mut best: Option<(C, u32)> = None;
    let mut unique = true;
    for c in candidates {
        let distance = (target ^ hash_of(c)).count_ones();
        if distance >= SIMHASH_HAMMING_THRESHOLD {
            continue;
        }
        match best {
            None => best = Some((c, distance)),
            Some((_, prev)) => {
                if distance < prev {
                    best = Some((c, distance));
                    unique = true;
                } else if distance == prev {
                    unique = false;
                }
            }
        }
    }
    best.and_then(|(c, _)| if unique { Some(c) } else { None })
}

/// Flatten a Value into individual SimHash features.
///
/// Map values are expanded so each entry becomes a separate feature (e.g., `tags.Environment`),
/// allowing SimHash to produce close hashes when only one map entry changes.
/// Non-map values use `deterministic_value_string` as the feature value.
pub(crate) fn flatten_value_for_simhash(
    prefix: &str,
    value: &Value,
    out: &mut std::collections::BTreeMap<String, String>,
    attribute_type: Option<&AttributeType>,
) {
    match value {
        Value::Concrete(ConcreteValue::Map(map)) => {
            for (k, v) in map {
                let key = format!("{}.{}", prefix, k);
                flatten_value_for_simhash(&key, v, out, None);
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let key = format!("{}[{}]", prefix, i);
                flatten_value_for_simhash(&key, item, out, None);
            }
        }
        _ => {
            out.insert(
                prefix.to_string(),
                canonical_enum_feature_string(value, attribute_type),
            );
        }
    }
}

/// Compute SimHash of a set of key-value attributes.
///
/// SimHash is a locality-sensitive hash: changing one attribute flips only a few bits,
/// so similar inputs produce similar hashes. This enables similarity-based reconciliation
/// using Hamming distance on the identifier alone.
pub(crate) fn compute_simhash<K: std::fmt::Display>(
    attributes: &std::collections::BTreeMap<K, String>,
) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut v = [0i32; 64];
    for (key, value) in attributes {
        let feature = format!("{}={}", key, value);
        let mut hasher = std::hash::DefaultHasher::new();
        feature.hash(&mut hasher);
        let hash = hasher.finish();
        for (i, count) in v.iter_mut().enumerate() {
            if (hash >> i) & 1 == 1 {
                *count += 1;
            } else {
                *count -= 1;
            }
        }
    }
    let mut result: u64 = 0;
    for (i, count) in v.iter().enumerate() {
        if *count > 0 {
            result |= 1 << i;
        }
    }
    result
}

/// Extract the hash portion from an anonymous resource identifier.
///
/// Supports both 8 hex chars (standard hash, u32) and 16 hex chars (SimHash, u64).
/// Identifier format: `{resource_type}_{hex}` (e.g., `ec2_eip_a3f2b1c8d79f1524`).
pub(crate) fn extract_hash_from_identifier(identifier: &str) -> Option<u64> {
    let hex_part = identifier.rsplit('_').next()?;
    match hex_part.len() {
        16 => u64::from_str_radix(hex_part, 16).ok(),
        8 => u32::from_str_radix(hex_part, 16).ok().map(|v| v as u64),
        _ => None,
    }
}

/// Build a SimHash over the combined set of provider identity values plus
/// a resource's user-specified attributes (non-`_`-prefixed, flattened).
///
/// This is the shared feature set used by both `compute_anonymous_identifiers`
/// (when the schema has no create-only attributes) and `compute_resource_simhash`
/// so the two always agree on what a resource's anonymous ID would be.
fn simhash_from_identity_and_resource(
    identity_values: &BTreeMap<String, String>,
    resource: &Resource,
    schema: Option<&ResourceSchema>,
) -> u64 {
    let mut simhash_values = identity_values.clone();
    for (key, value) in &resource.attributes {
        if key.starts_with('_') {
            continue;
        }
        let attribute_type = schema
            .and_then(|schema| schema.attributes.get(key))
            .map(|attr| &attr.attr_type);
        flatten_value_for_simhash(key, value, &mut simhash_values, attribute_type);
    }
    compute_simhash(&simhash_values)
}

/// Compute the SimHash `compute_anonymous_identifiers` would produce for a
/// single resource. Used by `detect_anonymous_to_named_renames` to recover the
/// anonymous ID of a resource that has since been wrapped in a `let` binding.
fn compute_resource_simhash(
    resource: &Resource,
    providers: &[ProviderConfig],
    registry: &SchemaRegistry,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> u64 {
    let mut identity_values: BTreeMap<String, String> = BTreeMap::new();
    if !resource.id.provider.is_empty() {
        let identity_attrs = identity_attributes_fn(&resource.id.provider);
        if let Some(pc) = providers.iter().find(|p| p.name == resource.id.provider) {
            for attr_name in &identity_attrs {
                if let Some(value) = pc.attributes.get(attr_name.as_str()) {
                    identity_values.insert(attr_name.clone(), deterministic_value_string(value));
                }
            }
        }
    }

    simhash_from_identity_and_resource(&identity_values, resource, registry.get_for(resource))
}

type ProviderConfigAttributeTypeFn<'a> = dyn Fn(&str, &str) -> Option<AttributeType> + 'a;

/// Compute stable identifiers for anonymous resources (those with empty ResourceId.name).
/// Uses create-only properties and provider identity attributes to generate a deterministic hash.
///
/// `identity_attributes_fn` takes a provider name and returns the list of identity attribute names
/// for that provider (e.g., `["region"]`).
pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
    registry: &SchemaRegistry,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Result<(), String> {
    compute_anonymous_identifiers_with_provider_config_types(
        resources,
        providers,
        registry,
        identity_attributes_fn,
        &|_, _| None,
    )
}

pub fn compute_anonymous_identifiers_with_provider_config_types(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
    registry: &SchemaRegistry,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
    provider_config_attribute_type_fn: &ProviderConfigAttributeTypeFn<'_>,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};

    // First pass: compute identifiers and detect collisions
    let mut computed: Vec<(usize, String)> = Vec::new();

    for (idx, resource) in resources.iter().enumerate() {
        if !resource.id.name.is_pending() {
            continue;
        }

        // Look up schema for this resource's provider (skip if no provider set)
        if resource.id.provider.is_empty() {
            continue;
        }
        let provider_name = &resource.id.provider;

        let Some(schema) = registry.get_for(resource) else {
            continue;
        };

        let create_only_attrs = schema.create_only_attributes();
        let schema_identity_attrs = schema.identity_attributes();

        // Collect identity attribute values (e.g., region) from provider config
        let mut identity_values: BTreeMap<String, String> = BTreeMap::new();
        let identity_attrs = identity_attributes_fn(provider_name);
        if let Some(pc) = providers.iter().find(|p| p.name == *provider_name) {
            for attr_name in &identity_attrs {
                if let Some(value) = pc.attributes.get(attr_name.as_str()) {
                    let attribute_type =
                        provider_config_attribute_type_fn(provider_name, attr_name);
                    identity_values.insert(
                        attr_name.clone(),
                        canonical_enum_feature_string(value, attribute_type.as_ref()),
                    );
                }
            }
        }

        // Collect create-only values in sorted order for deterministic hashing.
        // If no create-only properties exist or none are set, fall back to
        // all user-specified attributes.
        //
        // For prefixed attributes (e.g., bucket_name_prefix -> bucket_name),
        // hash the prefix value instead of the randomly generated name.
        // This ensures the anonymous identifier is stable across runs.
        let mut hash_values: BTreeMap<&str, String> = BTreeMap::new();
        for attr_name in &create_only_attrs {
            if let Some(prefix) = resource.prefixes.get(*attr_name) {
                // Use the prefix for hashing to produce a stable identifier
                hash_values.insert(attr_name, format!("Prefix({:?})", prefix));
            } else if let Some(value) = resource.get_attr(attr_name) {
                let attribute_type = schema
                    .attributes
                    .get(*attr_name)
                    .map(|attr| &attr.attr_type);
                hash_values.insert(
                    attr_name,
                    canonical_enum_feature_string(value, attribute_type),
                );
            }
        }
        // Also include schema-level identity attributes in the hash.
        // These distinguish resources that share create-only values but differ
        // in other key attributes (e.g., Route 53 RecordSet `type`).
        for attr_name in &schema_identity_attrs {
            if !hash_values.contains_key(attr_name)
                && let Some(value) = resource.get_attr(attr_name)
            {
                let attribute_type = schema
                    .attributes
                    .get(*attr_name)
                    .map(|attr| &attr.attr_type);
                hash_values.insert(
                    attr_name,
                    canonical_enum_feature_string(value, attribute_type),
                );
            }
        }

        let use_simhash = hash_values.is_empty();

        let hash_str = if use_simhash {
            // Use SimHash for locality-sensitive hashing: similar inputs produce
            // similar hashes, enabling Hamming distance reconciliation.
            let simhash =
                simhash_from_identity_and_resource(&identity_values, resource, Some(schema));
            format!("{:016x}", simhash)
        } else {
            // Use standard hash for create-only properties
            let mut hasher = std::hash::DefaultHasher::new();
            for (k, v) in &identity_values {
                k.hash(&mut hasher);
                v.hash(&mut hasher);
            }
            for (k, v) in &hash_values {
                k.hash(&mut hasher);
                v.hash(&mut hasher);
            }
            format!("{:08x}", hasher.finish() & 0xFFFFFFFF)
        };

        // Build identifier: provider_resource_type_hash (e.g., awscc_ec2_vpc_a3f2b1c8).
        // The resource_type segments carry PascalCase for the final type name
        // (e.g., "ec2.Vpc"); identifier names are snake_case values per the
        // naming-conventions rule, so lower each segment before joining.
        // The provider segment makes anonymous identifiers self-describe their
        // provider so plan output and state files distinguish e.g.
        // `aws.iam.RolePolicy` from `awscc.iam.RolePolicy` at a glance.
        let provider_snake = resource
            .id
            .provider
            .split('.')
            .map(crate::parser::pascal_to_snake)
            .collect::<Vec<_>>()
            .join("_");
        let type_snake = resource
            .id
            .resource_type
            .split('.')
            .map(crate::parser::pascal_to_snake)
            .collect::<Vec<_>>()
            .join("_");
        let bare_identifier = format!("{}_{}_{}", provider_snake, type_snake, hash_str);
        // If this anonymous resource lives inside a module instantiation,
        // surface the owning instance in the identifier (`<instance>.<bare>`).
        // The expander leaves anonymous resources `Pending` so this pass
        // still sees them; without the prefix, a `Pending` module-internal
        // resource would land at the same address as a top-level
        // `Pending` resource with matching create-only attrs (#2516).
        let identifier = match &resource.module_source {
            Some(crate::resource::ModuleSource::Module { instance, .. }) => {
                format!("{}.{}", instance, bare_identifier)
            }
            _ => bare_identifier,
        };

        computed.push((idx, identifier));
    }

    // Detect collisions
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (idx, identifier) in &computed {
        if let Some(first_idx) = seen.get(identifier) {
            return Err(format!(
                "Anonymous resource identifier collision: '{}' and '{}' produce the same identifier '{}'. \
                 Use `let` bindings to give them distinct names.",
                resources[*first_idx].id.display_type(),
                resources[*idx].id.display_type(),
                identifier,
            ));
        }
        seen.insert(identifier.clone(), *idx);
    }

    // Second pass: apply identifiers
    for (idx, identifier) in computed {
        let provider = resources[idx].id.provider.clone();
        let resource_type = resources[idx].id.resource_type.clone();
        let provider_instance = resources[idx].id.provider_instance.clone();
        resources[idx].id =
            ResourceId::with_provider(&provider, &resource_type, identifier, provider_instance);
    }

    Ok(())
}

/// State information needed for anonymous identifier reconciliation.
#[derive(Clone)]
pub struct AnonymousIdStateInfo {
    /// The resource name (anonymous identifier) stored in state
    pub name: String,
    /// Create-only attribute values from state, keyed by DSL attribute name
    pub create_only_values: HashMap<String, String>,
}

/// Reconcile anonymous resource identifiers with existing state.
///
/// When a create-only property changes on an anonymous resource, the computed
/// hash-based identifier changes. This function detects such cases by comparing
/// create-only property values and restores the original identifier from state
/// when at least one create-only property matches (partial match), allowing the
/// differ to generate a Replace effect instead of Create+Delete.
///
/// `find_state_by_type` takes (provider, resource_type) and returns all state
/// entries for that resource type with their create-only attribute values.
///
/// Returns a list of `(old_state_name, new_resource_name)` pairs that the
/// caller should apply to state-keyed maps. These are emitted when a SimHash
/// match identifies a state entry under an older identifier format (e.g.
/// `iam_role_policy_<hash>` from before the provider prefix was added) — the
/// resource keeps its freshly-computed new-format name and the wiring layer
/// re-keys the state entry instead of destroy+recreate.
pub fn reconcile_anonymous_identifiers(
    resources: &mut [Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
) -> Vec<(String, String)> {
    let mut renames: Vec<(String, String)> = Vec::new();
    for resource in resources.iter_mut() {
        if resource.id.name.is_pending() {
            continue;
        }

        // Skip let-bound (named) resources entirely. Reconciliation is only
        // meaningful for anonymous hash-derived identifiers. Named resources
        // should never be rebound to a different state entry.
        if resource.binding.is_some() {
            continue;
        }

        let Some(schema) = registry.get_for(resource) else {
            continue;
        };

        let create_only_attrs = schema.create_only_attributes();
        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);

        // If the resource's name already exists in state, no reconciliation is needed.
        if state_entries
            .iter()
            .any(|e| e.name == resource.id.name_str())
        {
            continue;
        }

        // Collect this resource's create-only values
        let mut resource_co_values: HashMap<&str, String> = HashMap::new();
        for attr_name in &create_only_attrs {
            if let Some(value) = resource.get_attr(attr_name) {
                let attribute_type = schema
                    .attributes
                    .get(*attr_name)
                    .map(|attr| &attr.attr_type);
                if let Some(value) = canonical_create_only_value_string(value, attribute_type) {
                    resource_co_values.insert(attr_name, value);
                }
            }
        }

        if create_only_attrs.is_empty() || resource_co_values.is_empty() {
            // No create-only properties or none set: use SimHash-based Hamming distance
            // matching to find the closest state entry.
            let Some(resource_hash) = extract_hash_from_identifier(resource.id.name_str()) else {
                continue;
            };

            let mut best_match: Option<(&str, u32)> = None;
            for entry in &state_entries {
                if entry.name == resource.id.name_str() {
                    continue;
                }
                let Some(state_hash) = extract_hash_from_identifier(&entry.name) else {
                    continue;
                };
                let distance = (resource_hash ^ state_hash).count_ones();
                if distance < SIMHASH_HAMMING_THRESHOLD
                    && (best_match.is_none() || distance < best_match.unwrap().1)
                {
                    best_match = Some((&entry.name, distance));
                }
            }

            if let Some((state_name, _)) = best_match {
                // The state entry may use an older identifier format (e.g.
                // pre-provider-prefix). Keep our freshly-computed new-format
                // name on the resource and record a rename so the wiring
                // layer can re-key the state entry.
                renames.push((state_name.to_string(), resource.id.name_str().to_string()));
            }
            continue;
        }

        // Collect all partial matches (at least one create-only property matches
        // and at least one differs). If there are multiple partial matches, skip
        // reconciliation to avoid rebinding to the wrong state entry.
        let mut partial_matches: Vec<&str> = Vec::new();
        for entry in &state_entries {
            if entry.name == resource.id.name_str() {
                // Same identifier, no reconciliation needed
                continue;
            }

            // Compare create-only values: count matches and mismatches
            let mut matched = 0;
            let mut mismatched = 0;
            for (attr, value) in &resource_co_values {
                if let Some(state_value) = entry.create_only_values.get(*attr) {
                    let attribute_type = schema.attributes.get(*attr).map(|attr| &attr.attr_type);
                    let state_value =
                        canonical_state_create_only_value_string(state_value, attribute_type);
                    if &state_value == value {
                        matched += 1;
                    } else {
                        mismatched += 1;
                    }
                }
            }

            // Partial match = same resource with changes to some create-only properties
            if matched > 0 && mismatched > 0 {
                partial_matches.push(&entry.name);
            }
        }

        // Only reconcile if there is exactly one partial match (unique best match).
        // Multiple partial matches are ambiguous - skip to avoid rebinding wrong.
        if partial_matches.len() == 1 {
            resource.id = ResourceId::with_provider(
                &resource.id.provider,
                &resource.id.resource_type,
                partial_matches[0],
                resource.id.provider_instance.clone(),
            );
        }
    }
    renames
}

/// Detect let-bound (named) resources that were previously anonymous.
///
/// When a user converts an anonymous resource to a `let`-bound resource while
/// preserving the same create-only attributes, the old state entry (with a
/// hash-derived name) doesn't match the new binding name. Without this
/// detection the differ treats the change as delete + create, which for
/// destructive resources (e.g., `awscc.sso.Instance`) can wipe out live data.
///
/// Returns a list of `(old_anonymous_name, new_binding_name)` pairs for each
/// matched rename. Callers should transfer state entries from the old name to
/// the new name before running the differ (similar to `materialize_moved_states`).
///
/// Matching rules:
/// 1. Only `let`-bound resources are candidates (those with `binding.is_some()`)
/// 2. The resource's binding name must not already exist in state
/// 3. For resources whose schema has create-only attributes: there must be
///    exactly one orphaned anonymous state entry whose create-only attribute
///    values all match the new resource (ambiguous matches are skipped)
/// 4. For resources with no create-only attributes (e.g., `awscc.sso.Instance`):
///    fall back to SimHash Hamming-distance matching, using the same SimHash
///    that `compute_anonymous_identifiers` would have produced. This requires
///    `providers` and `identity_attributes_fn` so identity values (e.g.,
///    region) contribute to the hash just like they did at creation time.
///
/// An "orphaned" state entry is one whose name is not used by any current DSL
/// resource (so it would otherwise appear as a Delete in the plan).
pub fn detect_anonymous_to_named_renames(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
    providers: &[ProviderConfig],
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Vec<(ResourceId, ResourceId)> {
    // Collect the set of resource names currently used in the DSL per
    // (provider, resource_type). Any state entry not in this set is an orphan.
    let mut used_names: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for resource in resources {
        let key = (
            resource.id.provider.clone(),
            resource.id.resource_type.clone(),
        );
        used_names
            .entry(key)
            .or_default()
            .insert(resource.id.name_str().to_string());
    }

    let mut renames: Vec<(ResourceId, ResourceId)> = Vec::new();

    for resource in resources {
        // Only rename let-bound resources whose binding was previously anonymous.
        if resource.binding.is_none() {
            continue;
        }

        let Some(schema) = registry.get_for(resource) else {
            continue;
        };

        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);

        // Skip if the binding name already exists in state — nothing to rename.
        if state_entries
            .iter()
            .any(|e| e.name == resource.id.name_str())
        {
            continue;
        }

        let used_in_dsl = used_names
            .get(&(
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            ))
            .cloned()
            .unwrap_or_default();

        // Collect this resource's create-only values (may be empty if the
        // schema has no create-only attributes or none are set).
        let create_only_attrs = schema.create_only_attributes();
        let mut resource_co_values: HashMap<&str, String> = HashMap::new();
        for attr_name in &create_only_attrs {
            if let Some(Value::Concrete(ConcreteValue::String(v))) = resource.get_attr(attr_name) {
                resource_co_values.insert(attr_name, v.clone());
            }
        }

        let matched_name: Option<&str> = if !resource_co_values.is_empty() {
            // Create-only path: find orphaned entries whose create-only values all match.
            let mut matches: Vec<&str> = Vec::new();
            for entry in &state_entries {
                if used_in_dsl.contains(&entry.name) {
                    continue;
                }
                if extract_hash_from_identifier(&entry.name).is_none() {
                    continue;
                }
                let all_match = resource_co_values.iter().all(|(attr, value)| {
                    entry
                        .create_only_values
                        .get(*attr)
                        .is_some_and(|v| v == value)
                });
                if all_match {
                    matches.push(&entry.name);
                }
            }
            // Only rename on a unique match to avoid rebinding the wrong entry.
            if matches.len() == 1 {
                Some(matches[0])
            } else {
                None
            }
        } else {
            // SimHash fallback (rule 4 in the function doc). Pick the orphan
            // entry closest to the computed SimHash; tie → ambiguous, skip.
            let resource_hash =
                compute_resource_simhash(resource, providers, registry, identity_attributes_fn);
            let candidates = state_entries
                .iter()
                .filter(|e| !used_in_dsl.contains(&e.name))
                // Only consider state entries written via the SimHash path
                // (16-hex suffix). 8-hex entries come from the create-only
                // hash scheme and are meaningless to XOR with a 64-bit SimHash.
                .filter(|e| e.name.rsplit('_').next().map(str::len) == Some(16))
                .filter_map(|e| {
                    extract_hash_from_identifier(&e.name).map(|h| (e.name.as_str(), h))
                });
            closest_unique_simhash_match(resource_hash, candidates, |(_, h)| h)
                .map(|(name, _)| name)
        };

        if let Some(name) = matched_name {
            let from = ResourceId::with_provider(
                &resource.id.provider,
                &resource.id.resource_type,
                name,
                resource.id.provider_instance.clone(),
            );
            renames.push((from, resource.id.clone()));
        }
    }

    renames
}

#[cfg(test)]
mod tests;
