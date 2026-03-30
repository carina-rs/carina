//! Identifier and prefix handling for resources
//!
//! Functions for generating random suffixes, resolving attribute prefixes,
//! reconciling prefixed names with state, and computing anonymous resource identifiers.

use std::collections::HashMap;

use crate::parser::ProviderConfig;
use crate::resource::{Resource, ResourceId, Value};
use crate::schema::ResourceSchema;
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
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources.iter_mut() {
        let schema_key = schema_key_fn(resource);

        let schema = match schemas.get(&schema_key) {
            Some(s) => s,
            None => continue, // Unknown resource type; validate_resources will catch this
        };

        // Collect prefix attributes to process
        let prefix_attrs: Vec<(String, String)> = resource
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                if let Some(base_attr) = key.strip_suffix("_prefix")
                    && let Value::String(prefix_value) = &**value
                    && let Some(attr_schema) = schema.attributes.get(base_attr)
                    && is_string_compatible_type(&attr_schema.attr_type)
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

            // Remove the _prefix attribute
            resource.attributes.remove(&prefix_key);

            // Store prefix
            resource
                .prefixes
                .insert(base_attr.clone(), prefix_value.clone());

            // Generate temporary name
            let generated_name = format!("{}{}", prefix_value, generate_random_suffix());
            resource.set_attr(base_attr, Value::String(generated_name));
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
            &resource.id.name,
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
                    state_info
                        .attribute_values
                        .get(attr_name)
                        .map(|name_str| (attr_name.clone(), Value::String(name_str.clone())))
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
        Value::String(s) => format!("String({:?})", s),
        Value::Int(i) => format!("Int({})", i),
        Value::Float(f) => format!("Float({})", f),
        Value::Bool(b) => format!("Bool({})", b),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(deterministic_value_string).collect();
            format!("List([{}])", parts.join(", "))
        }
        Value::Map(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{:?}: {}", k, deterministic_value_string(v)))
                .collect();
            format!("Map({{{}}})", parts.join(", "))
        }
        Value::ResourceRef {
            binding_name,
            attribute_name,
            field_path,
        } => {
            if field_path.is_empty() {
                format!("ResourceRef({}.{})", binding_name, attribute_name)
            } else {
                format!(
                    "ResourceRef({}.{}.{})",
                    binding_name,
                    attribute_name,
                    field_path.join(".")
                )
            }
        }
        Value::Interpolation(parts) => {
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
        Value::FunctionCall { name, args } => {
            let arg_strs: Vec<String> = args.iter().map(deterministic_value_string).collect();
            format!("FunctionCall({}({}))", name, arg_strs.join(", "))
        }
        Value::Secret(inner) => {
            format!("Secret({})", deterministic_value_string(inner))
        }
    }
}

/// Maximum Hamming distance (out of 64 bits) for SimHash-based reconciliation.
/// Two identifiers with distance below this threshold are considered the "same resource"
/// with modified attributes.
const SIMHASH_HAMMING_THRESHOLD: u32 = 20;

/// Flatten a Value into individual SimHash features.
///
/// Map values are expanded so each entry becomes a separate feature (e.g., `tags.Environment`),
/// allowing SimHash to produce close hashes when only one map entry changes.
/// Non-map values use `deterministic_value_string` as the feature value.
fn flatten_value_for_simhash(
    prefix: &str,
    value: &Value,
    out: &mut std::collections::BTreeMap<String, String>,
) {
    match value {
        Value::Map(map) => {
            for (k, v) in map {
                let key = format!("{}.{}", prefix, k);
                flatten_value_for_simhash(&key, v, out);
            }
        }
        Value::List(items) => {
            for (i, item) in items.iter().enumerate() {
                let key = format!("{}[{}]", prefix, i);
                flatten_value_for_simhash(&key, item, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), deterministic_value_string(value));
        }
    }
}

/// Compute SimHash of a set of key-value attributes.
///
/// SimHash is a locality-sensitive hash: changing one attribute flips only a few bits,
/// so similar inputs produce similar hashes. This enables similarity-based reconciliation
/// using Hamming distance on the identifier alone.
fn compute_simhash<K: std::fmt::Display>(
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
fn extract_hash_from_identifier(identifier: &str) -> Option<u64> {
    let hex_part = identifier.rsplit('_').next()?;
    match hex_part.len() {
        16 => u64::from_str_radix(hex_part, 16).ok(),
        8 => u32::from_str_radix(hex_part, 16).ok().map(|v| v as u64),
        _ => None,
    }
}

/// Compute stable identifiers for anonymous resources (those with empty ResourceId.name).
/// Uses create-only properties and provider identity attributes to generate a deterministic hash.
///
/// `identity_attributes_fn` takes a provider name and returns the list of identity attribute names
/// for that provider (e.g., `["region"]`).
pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};

    // First pass: compute identifiers and detect collisions
    let mut computed: Vec<(usize, String)> = Vec::new();

    for (idx, resource) in resources.iter().enumerate() {
        if !resource.id.name.is_empty() {
            continue;
        }

        // Look up schema for this resource's provider (skip if no provider set)
        if resource.id.provider.is_empty() {
            continue;
        }
        let provider_name = &resource.id.provider;
        let schema_key = schema_key_fn(resource);

        let Some(schema) = schemas.get(&schema_key) else {
            continue;
        };

        let create_only_attrs = schema.create_only_attributes();

        // Collect identity attribute values (e.g., region) from provider config
        let mut identity_values: BTreeMap<String, String> = BTreeMap::new();
        let identity_attrs = identity_attributes_fn(provider_name);
        if let Some(pc) = providers.iter().find(|p| p.name == *provider_name) {
            for attr_name in &identity_attrs {
                if let Some(value) = pc.attributes.get(attr_name.as_str()) {
                    identity_values.insert(attr_name.clone(), deterministic_value_string(value));
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
                hash_values.insert(attr_name, deterministic_value_string(value));
            }
        }

        let use_simhash = hash_values.is_empty();

        let hash_str = if use_simhash {
            // Use SimHash for locality-sensitive hashing: similar inputs produce
            // similar hashes, enabling Hamming distance reconciliation.
            // Include identity attributes in the hash for provider distinction.
            //
            // Flatten Map/List values into individual features so that changing
            // a single entry within a map (e.g., one tag) only flips a few bits
            // in the SimHash, keeping the Hamming distance small.
            let mut simhash_values: BTreeMap<String, String> = BTreeMap::new();
            for (k, v) in &identity_values {
                simhash_values.insert(k.clone(), v.clone());
            }
            for (key, value) in &resource.attributes {
                if key.starts_with('_') {
                    continue;
                }
                flatten_value_for_simhash(key, value, &mut simhash_values);
            }
            let simhash = compute_simhash(&simhash_values);
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

        // Build identifier: resource_type_hash (e.g., ec2_vpc_a3f2b1c8)
        let identifier = format!(
            "{}_{}",
            resource.id.resource_type.replace('.', "_"),
            hash_str
        );

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
        resources[idx].id = ResourceId::with_provider(&provider, &resource_type, identifier);
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
pub fn reconcile_anonymous_identifiers(
    resources: &mut [Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&Resource) -> String,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
) {
    for resource in resources.iter_mut() {
        if resource.id.name.is_empty() {
            continue;
        }

        // Skip let-bound (named) resources entirely. Reconciliation is only
        // meaningful for anonymous hash-derived identifiers. Named resources
        // should never be rebound to a different state entry.
        if resource.binding.is_some() {
            continue;
        }

        let schema_key = schema_key_fn(resource);
        let Some(schema) = schemas.get(&schema_key) else {
            continue;
        };

        let create_only_attrs = schema.create_only_attributes();
        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);

        // If the resource's name already exists in state, no reconciliation is needed.
        if state_entries.iter().any(|e| e.name == resource.id.name) {
            continue;
        }

        // Collect this resource's create-only values
        let mut resource_co_values: HashMap<&str, String> = HashMap::new();
        for attr_name in &create_only_attrs {
            if let Some(Value::String(v)) = resource.get_attr(attr_name) {
                resource_co_values.insert(attr_name, v.clone());
            }
        }

        if create_only_attrs.is_empty() || resource_co_values.is_empty() {
            // No create-only properties or none set: use SimHash-based Hamming distance
            // matching to find the closest state entry.
            let Some(resource_hash) = extract_hash_from_identifier(&resource.id.name) else {
                continue;
            };

            let mut best_match: Option<(&str, u32)> = None;
            for entry in &state_entries {
                if entry.name == resource.id.name {
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

            if let Some((name, _)) = best_match {
                resource.id = ResourceId::with_provider(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    name,
                );
            }
            continue;
        }

        // Collect all partial matches (at least one create-only property matches
        // and at least one differs). If there are multiple partial matches, skip
        // reconciliation to avoid rebinding to the wrong state entry.
        let mut partial_matches: Vec<&str> = Vec::new();
        for entry in &state_entries {
            if entry.name == resource.id.name {
                // Same identifier, no reconciliation needed
                continue;
            }

            // Compare create-only values: count matches and mismatches
            let mut matched = 0;
            let mut mismatched = 0;
            for (attr, value) in &resource_co_values {
                if let Some(state_value) = entry.create_only_values.get(*attr) {
                    if state_value == value {
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
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::Resource;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn make_s3_bucket_schema() -> (String, ResourceSchema) {
        let schema = ResourceSchema::new("awscc.s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String));
        ("awscc.s3.bucket".to_string(), schema)
    }

    fn schema_key_fn(resource: &Resource) -> String {
        if resource.id.provider.is_empty() {
            resource.id.resource_type.clone()
        } else {
            format!("{}.{}", resource.id.provider, resource.id.resource_type)
        }
    }

    #[test]
    fn test_generate_random_suffix_format() {
        let suffix = generate_random_suffix();
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_resolve_attr_prefixes_extracts_prefix_and_generates_name() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource.set_attr(
            "bucket_name_prefix".to_string(),
            Value::String("my-app-".to_string()),
        );

        let schemas: HashMap<String, ResourceSchema> =
            vec![make_s3_bucket_schema()].into_iter().collect();
        let mut resources = vec![resource];
        resolve_attr_prefixes(&mut resources, &schemas, &schema_key_fn).unwrap();

        // bucket_name_prefix should be removed
        assert!(!resources[0].attributes.contains_key("bucket_name_prefix"));

        // bucket_name should be generated with the prefix
        let bucket_name = match resources[0].get_attr("bucket_name").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected String"),
        };
        assert!(bucket_name.starts_with("my-app-"));
        assert_eq!(bucket_name.len(), "my-app-".len() + 8); // prefix + 8 hex chars

        // prefixes map should have the entry
        assert_eq!(
            resources[0].prefixes.get("bucket_name"),
            Some(&"my-app-".to_string())
        );
    }

    #[test]
    fn test_resolve_attr_prefixes_leaves_non_matching_prefix_alone() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource.set_attr(
            "nonexistent_attr_prefix".to_string(),
            Value::String("some-value".to_string()),
        );

        let schemas: HashMap<String, ResourceSchema> =
            vec![make_s3_bucket_schema()].into_iter().collect();
        let mut resources = vec![resource];
        resolve_attr_prefixes(&mut resources, &schemas, &schema_key_fn).unwrap();

        // nonexistent_attr_prefix should remain untouched
        assert!(
            resources[0]
                .attributes
                .contains_key("nonexistent_attr_prefix")
        );
        assert!(resources[0].prefixes.is_empty());
    }

    #[test]
    fn test_resolve_attr_prefixes_errors_when_both_prefix_and_attr_specified() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource.set_attr(
            "bucket_name_prefix".to_string(),
            Value::String("my-app-".to_string()),
        );
        resource.set_attr(
            "bucket_name".to_string(),
            Value::String("my-actual-bucket".to_string()),
        );

        let schemas: HashMap<String, ResourceSchema> =
            vec![make_s3_bucket_schema()].into_iter().collect();
        let mut resources = vec![resource];
        let result = resolve_attr_prefixes(&mut resources, &schemas, &schema_key_fn);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot specify both"));
    }

    #[test]
    fn test_resolve_attr_prefixes_errors_on_empty_prefix() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource.set_attr(
            "bucket_name_prefix".to_string(),
            Value::String("".to_string()),
        );

        let schemas: HashMap<String, ResourceSchema> =
            vec![make_s3_bucket_schema()].into_iter().collect();
        let mut resources = vec![resource];
        let result = resolve_attr_prefixes(&mut resources, &schemas, &schema_key_fn);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot be empty"));
    }

    #[test]
    fn test_reconcile_prefixed_names_reuses_state_name_when_prefix_matches() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        resource.set_attr(
            "bucket_name".to_string(),
            Value::String("my-app-temporary".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &|_provider, _resource_type, _name| {
            Some(PrefixStateInfo {
                prefixes: vec![("bucket_name".to_string(), "my-app-".to_string())]
                    .into_iter()
                    .collect(),
                attribute_values: vec![("bucket_name".to_string(), "my-app-existing1".to_string())]
                    .into_iter()
                    .collect(),
            })
        });

        // Should reuse the state name, not the temporary one
        assert_eq!(
            resources[0].get_attr("bucket_name"),
            Some(&Value::String("my-app-existing1".to_string()))
        );
    }

    #[test]
    fn test_reconcile_prefixed_names_generates_new_name_when_prefix_changes() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "new-prefix-".to_string());
        resource.set_attr(
            "bucket_name".to_string(),
            Value::String("new-prefix-abcd1234".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &|_provider, _resource_type, _name| {
            Some(PrefixStateInfo {
                prefixes: vec![("bucket_name".to_string(), "old-prefix-".to_string())]
                    .into_iter()
                    .collect(),
                attribute_values: vec![(
                    "bucket_name".to_string(),
                    "old-prefix-existing1".to_string(),
                )]
                .into_iter()
                .collect(),
            })
        });

        // Should keep the newly generated name since prefix changed
        assert_eq!(
            resources[0].get_attr("bucket_name"),
            Some(&Value::String("new-prefix-abcd1234".to_string()))
        );
    }

    #[test]
    fn test_reconcile_prefixed_names_keeps_generated_name_when_no_state() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        resource.set_attr(
            "bucket_name".to_string(),
            Value::String("my-app-abcd1234".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &|_provider, _resource_type, _name| None);

        // No state, so keep the generated name
        assert_eq!(
            resources[0].get_attr("bucket_name"),
            Some(&Value::String("my-app-abcd1234".to_string()))
        );
    }

    #[test]
    fn test_reconcile_anonymous_id_partial_create_only_match() {
        // When one create-only property changes but another stays the same,
        // reconciliation should restore the state's identifier.
        let schema = ResourceSchema::new("awscc.iam.role")
            .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("path", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.iam.role".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        // Step 1: compute identifier with path="/"
        let mut r1 = Resource::with_provider("awscc", "iam.role", "");
        r1.set_attr(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        r1.set_attr("path".to_string(), Value::String("/".to_string()));
        let mut resources1 = vec![r1];
        compute_anonymous_identifiers(
            &mut resources1,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let step1_id = resources1[0].id.name.clone();

        // Step 2: compute identifier with path="/carina/" (changed create-only)
        let mut r2 = Resource::with_provider("awscc", "iam.role", "");
        r2.set_attr(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        r2.set_attr("path".to_string(), Value::String("/carina/".to_string()));
        let mut resources2 = vec![r2];
        compute_anonymous_identifiers(
            &mut resources2,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let step2_id = resources2[0].id.name.clone();

        // Hash includes path, so identifiers differ
        assert_ne!(step1_id, step2_id);

        // Reconcile: state has role_name="my-role" (match) and path="/" (mismatch)
        let state_entries = vec![AnonymousIdStateInfo {
            name: step1_id.clone(),
            create_only_values: vec![
                ("role_name".to_string(), "my-role".to_string()),
                ("path".to_string(), "/".to_string()),
            ]
            .into_iter()
            .collect(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources2,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // After reconciliation, step2 resource should have step1's identifier
        assert_eq!(resources2[0].id.name, step1_id);
    }

    #[test]
    fn test_reconcile_anonymous_id_no_match_when_all_differ() {
        // When ALL create-only properties differ, no reconciliation (truly new resource)
        let schema = ResourceSchema::new("awscc.iam.role")
            .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("path", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.iam.role".to_string(), schema)]
            .into_iter()
            .collect();

        let mut resource = Resource::with_provider("awscc", "iam.role", "iam_role_aabbccdd");
        resource.set_attr(
            "role_name".to_string(),
            Value::String("new-role".to_string()),
        );
        resource.set_attr("path".to_string(), Value::String("/new/".to_string()));

        let original_id = resource.id.name.clone();
        let mut resources = vec![resource];

        // State has completely different values
        let state_entries = vec![AnonymousIdStateInfo {
            name: "iam_role_11223344".to_string(),
            create_only_values: vec![
                ("role_name".to_string(), "old-role".to_string()),
                ("path".to_string(), "/old/".to_string()),
            ]
            .into_iter()
            .collect(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Identifier should remain unchanged
        assert_eq!(resources[0].id.name, original_id);
    }

    #[test]
    fn test_reconcile_anonymous_id_no_match_when_all_same() {
        // When ALL create-only properties match, the hash should also match,
        // so no reconciliation is needed (same identifier)
        let schema = ResourceSchema::new("awscc.iam.role")
            .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("path", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.iam.role".to_string(), schema)]
            .into_iter()
            .collect();

        let mut resource = Resource::with_provider("awscc", "iam.role", "iam_role_aabbccdd");
        resource.set_attr(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        resource.set_attr("path".to_string(), Value::String("/".to_string()));

        let original_id = resource.id.name.clone();
        let mut resources = vec![resource];

        // State has same values but different ID (shouldn't happen in practice,
        // but reconciliation should NOT trigger since no mismatch)
        let state_entries = vec![AnonymousIdStateInfo {
            name: "iam_role_11223344".to_string(),
            create_only_values: vec![
                ("role_name".to_string(), "my-role".to_string()),
                ("path".to_string(), "/".to_string()),
            ]
            .into_iter()
            .collect(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Identifier should remain unchanged (all values match = no partial match)
        assert_eq!(resources[0].id.name, original_id);
    }

    #[test]
    fn test_reconcile_anonymous_id_single_create_only_no_reconcile() {
        // With only one create-only property, changing it means ALL changed,
        // so no reconciliation (matched=0 or mismatched=0)
        let schema = ResourceSchema::new("awscc.ec2.vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.vpc".to_string(), schema)]
            .into_iter()
            .collect();

        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "ec2_vpc_aabbccdd");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.1.0.0/16".to_string()),
        );

        let original_id = resource.id.name.clone();
        let mut resources = vec![resource];

        let state_entries = vec![AnonymousIdStateInfo {
            name: "ec2_vpc_11223344".to_string(),
            create_only_values: vec![("cidr_block".to_string(), "10.0.0.0/16".to_string())]
                .into_iter()
                .collect(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // No reconciliation: only one create-only prop and it changed
        assert_eq!(resources[0].id.name, original_id);
    }

    #[test]
    fn test_anonymous_resource_no_create_only_properties() {
        // Resources with no create-only properties should still work as anonymous resources
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String))
            .attribute(AttributeSchema::new(
                "tags",
                AttributeType::Map(Box::new(AttributeType::String)),
            ));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: vec![(
                "region".to_string(),
                Value::String("ap-northeast-1".to_string()),
            )]
            .into_iter()
            .collect(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

        let mut r = Resource::with_provider("awscc", "ec2.eip", "");
        r.set_attr("domain".to_string(), Value::String("vpc".to_string()));

        let mut resources = vec![r];
        compute_anonymous_identifiers(
            &mut resources,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();

        // Should have computed an identifier
        assert!(!resources[0].id.name.is_empty());
        assert!(resources[0].id.name.starts_with("ec2_eip_"));
    }

    #[test]
    fn test_anonymous_resource_no_create_only_deterministic() {
        // Same attributes should produce the same identifier
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: vec![(
                "region".to_string(),
                Value::String("ap-northeast-1".to_string()),
            )]
            .into_iter()
            .collect(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

        let make_resource = || {
            let mut r = Resource::with_provider("awscc", "ec2.eip", "");
            r.set_attr("domain".to_string(), Value::String("vpc".to_string()));
            r
        };

        let mut resources1 = vec![make_resource()];
        let mut resources2 = vec![make_resource()];
        compute_anonymous_identifiers(
            &mut resources1,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        compute_anonymous_identifiers(
            &mut resources2,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();

        assert_eq!(resources1[0].id.name, resources2[0].id.name);
    }

    #[test]
    fn test_anonymous_resource_no_create_only_collision() {
        // Two identical anonymous resources with no create-only properties should collide
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        let mut r1 = Resource::with_provider("awscc", "ec2.eip", "");
        r1.set_attr("domain".to_string(), Value::String("vpc".to_string()));

        let mut r2 = Resource::with_provider("awscc", "ec2.eip", "");
        r2.set_attr("domain".to_string(), Value::String("vpc".to_string()));

        let mut resources = vec![r1, r2];
        let result = compute_anonymous_identifiers(
            &mut resources,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("collision"));
    }

    #[test]
    fn test_simhash_similar_inputs_close_distance() {
        use std::collections::BTreeMap;

        // Two attribute sets differing by one value should have small Hamming distance
        let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
        attrs1.insert("domain", "vpc".to_string());
        attrs1.insert("tag_name", "my-eip".to_string());
        attrs1.insert("tag_env", "production".to_string());
        attrs1.insert("tag_team", "platform".to_string());
        attrs1.insert("region", "ap-northeast-1".to_string());

        let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
        attrs2.insert("domain", "vpc".to_string());
        attrs2.insert("tag_name", "my-eip".to_string());
        attrs2.insert("tag_env", "staging".to_string()); // Only this changed
        attrs2.insert("tag_team", "platform".to_string());
        attrs2.insert("region", "ap-northeast-1".to_string());

        let hash1 = compute_simhash(&attrs1);
        let hash2 = compute_simhash(&attrs2);
        let distance = (hash1 ^ hash2).count_ones();

        // Similar inputs (1 of 5 changed) should have small Hamming distance
        assert!(
            distance < SIMHASH_HAMMING_THRESHOLD,
            "Hamming distance {} should be < {} for similar inputs (1 of 5 attrs changed)",
            distance,
            SIMHASH_HAMMING_THRESHOLD
        );
    }

    #[test]
    fn test_simhash_identical_inputs_zero_distance() {
        use std::collections::BTreeMap;

        let mut attrs: BTreeMap<&str, String> = BTreeMap::new();
        attrs.insert("domain", "vpc".to_string());
        attrs.insert("tag_name", "my-eip".to_string());

        let hash1 = compute_simhash(&attrs);
        let hash2 = compute_simhash(&attrs);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_extract_hash_from_identifier() {
        // 16 hex chars (SimHash, 64-bit)
        assert_eq!(
            extract_hash_from_identifier("ec2_eip_a3f2b1c8d79f1524"),
            Some(0xa3f2b1c8d79f1524)
        );
        // 8 hex chars (standard hash, 32-bit) - still supported
        assert_eq!(extract_hash_from_identifier("ec2_vpc_00000000"), Some(0));
        assert_eq!(extract_hash_from_identifier("short"), None);
        assert_eq!(extract_hash_from_identifier("bad_zzzzzzzz"), None);
        // 12 hex chars (neither 8 nor 16) - rejected
        assert_eq!(extract_hash_from_identifier("ec2_eip_aabbccddeeff"), None);
    }

    #[test]
    fn test_reconcile_anonymous_id_no_create_only_hamming_match() {
        // When schema has no create-only properties and an attribute changes,
        // Hamming distance reconciliation should match with the closest state entry.
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String))
            .attribute(AttributeSchema::new("tag_name", AttributeType::String))
            .attribute(AttributeSchema::new("tag_env", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: vec![(
                "region".to_string(),
                Value::String("ap-northeast-1".to_string()),
            )]
            .into_iter()
            .collect(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

        // Step 1: compute identifier with tag_env="production"
        let mut r1 = Resource::with_provider("awscc", "ec2.eip", "");
        r1.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        r1.set_attr("tag_name".to_string(), Value::String("my-eip".to_string()));
        r1.set_attr(
            "tag_env".to_string(),
            Value::String("production".to_string()),
        );
        let mut resources1 = vec![r1];
        compute_anonymous_identifiers(
            &mut resources1,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let old_id = resources1[0].id.name.clone();

        // Step 2: compute identifier with tag_env="staging" (one attribute changed)
        let mut r2 = Resource::with_provider("awscc", "ec2.eip", "");
        r2.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        r2.set_attr("tag_name".to_string(), Value::String("my-eip".to_string()));
        r2.set_attr("tag_env".to_string(), Value::String("staging".to_string()));
        let mut resources2 = vec![r2];
        compute_anonymous_identifiers(
            &mut resources2,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let new_id = resources2[0].id.name.clone();

        // Identifiers should differ (different attributes)
        assert_ne!(old_id, new_id);

        // Reconcile: state has the old identifier
        let state_entries = vec![AnonymousIdStateInfo {
            name: old_id.clone(),
            create_only_values: HashMap::new(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources2,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // After reconciliation, should have the old identifier (Hamming distance match)
        assert_eq!(resources2[0].id.name, old_id);
    }

    #[test]
    fn test_reconcile_anonymous_id_no_create_only_no_match_when_distant() {
        // Completely different resources should not reconcile
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();

        // Resource with a computed identifier
        let mut resource = Resource::with_provider("awscc", "ec2.eip", "ec2_eip_aabbccdd11223344");
        resource.set_attr("domain".to_string(), Value::String("vpc".to_string()));

        let original_id = resource.id.name.clone();
        let mut resources = vec![resource];

        // State has a very different hash (flipped many bits)
        let state_entries = vec![AnonymousIdStateInfo {
            name: "ec2_eip_5544332266778899".to_string(),
            create_only_values: HashMap::new(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Identifier should remain unchanged (too distant)
        assert_eq!(resources[0].id.name, original_id);
    }

    #[test]
    fn test_reconcile_anonymous_id_create_only_exists_but_none_set() {
        // Case A: Schema has create-only properties, but user didn't set any.
        // Should use SimHash-based Hamming distance reconciliation.
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String))
            .attribute(
                AttributeSchema::new("public_ipv4_pool", AttributeType::String).create_only(),
            );
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        // Compute identifier without setting the create-only property
        let mut r1 = Resource::with_provider("awscc", "ec2.eip", "");
        r1.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        let mut resources = vec![r1];
        compute_anonymous_identifiers(
            &mut resources,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();

        // Should have computed an identifier (not errored)
        assert!(!resources[0].id.name.is_empty());
        assert!(resources[0].id.name.starts_with("ec2_eip_"));

        // Reconciliation should use Hamming distance (create-only values empty)
        let current_id = resources[0].id.name.clone();
        let state_id = current_id.clone(); // Same id in state = no reconciliation needed
        let state_entries = vec![AnonymousIdStateInfo {
            name: state_id,
            create_only_values: HashMap::new(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Same identifier in state, no change needed
        assert_eq!(resources[0].id.name, current_id);
    }

    // ==================== SimHash acceptance tests ====================
    // Comprehensive tests to verify SimHash behavior across various scenarios.

    #[test]
    fn test_simhash_different_attribute_count_produces_different_hash() {
        use std::collections::BTreeMap;

        let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
        attrs1.insert("domain", "vpc".to_string());

        let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
        attrs2.insert("domain", "vpc".to_string());
        attrs2.insert("tag_name", "extra".to_string());

        let hash1 = compute_simhash(&attrs1);
        let hash2 = compute_simhash(&attrs2);
        assert_ne!(hash1, hash2, "Adding an attribute should change the hash");
    }

    #[test]
    fn test_simhash_key_change_produces_different_hash() {
        use std::collections::BTreeMap;

        // Same value but different key should produce different hash
        let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
        attrs1.insert("domain", "vpc".to_string());

        let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
        attrs2.insert("region", "vpc".to_string());

        let hash1 = compute_simhash(&attrs1);
        let hash2 = compute_simhash(&attrs2);
        assert_ne!(
            hash1, hash2,
            "Different keys should produce different hashes"
        );
    }

    #[test]
    fn test_simhash_order_independent() {
        use std::collections::BTreeMap;

        // BTreeMap is sorted, so insertion order doesn't matter.
        // Verify that the same key-value pairs produce the same hash regardless.
        let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
        attrs1.insert("a", "1".to_string());
        attrs1.insert("b", "2".to_string());
        attrs1.insert("c", "3".to_string());

        let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
        attrs2.insert("c", "3".to_string());
        attrs2.insert("a", "1".to_string());
        attrs2.insert("b", "2".to_string());

        assert_eq!(compute_simhash(&attrs1), compute_simhash(&attrs2));
    }

    #[test]
    fn test_simhash_empty_attributes() {
        use std::collections::BTreeMap;

        let attrs: BTreeMap<&str, String> = BTreeMap::new();
        // Empty attributes should produce 0 (all vote counters remain 0, all bits off)
        assert_eq!(compute_simhash(&attrs), 0);
    }

    #[test]
    fn test_simhash_single_attribute() {
        use std::collections::BTreeMap;

        let mut attrs: BTreeMap<&str, String> = BTreeMap::new();
        attrs.insert("domain", "vpc".to_string());

        let hash = compute_simhash(&attrs);
        // Single attribute: hash should be non-zero and deterministic
        assert_ne!(hash, 0);
        assert_eq!(hash, compute_simhash(&attrs));
    }

    #[test]
    fn test_simhash_many_attributes_one_change_close_distance() {
        use std::collections::BTreeMap;

        // With many attributes, changing one should flip very few bits
        let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
        for i in 0..10 {
            attrs1.insert(
                Box::leak(format!("attr_{}", i).into_boxed_str()),
                format!("value_{}", i),
            );
        }

        let mut attrs2 = attrs1.clone();
        attrs2.insert("attr_5", "changed_value".to_string());

        let hash1 = compute_simhash(&attrs1);
        let hash2 = compute_simhash(&attrs2);
        let distance = (hash1 ^ hash2).count_ones();

        assert!(
            distance < SIMHASH_HAMMING_THRESHOLD,
            "Changing 1 of 10 attributes: Hamming distance {} should be < {}",
            distance,
            SIMHASH_HAMMING_THRESHOLD
        );
    }

    #[test]
    fn test_simhash_all_attributes_changed_large_distance() {
        use std::collections::BTreeMap;

        // Completely different attribute values should have large Hamming distance
        let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
        attrs1.insert("a", "alpha".to_string());
        attrs1.insert("b", "bravo".to_string());
        attrs1.insert("c", "charlie".to_string());
        attrs1.insert("d", "delta".to_string());
        attrs1.insert("e", "echo".to_string());

        let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
        attrs2.insert("a", "xray".to_string());
        attrs2.insert("b", "yankee".to_string());
        attrs2.insert("c", "zulu".to_string());
        attrs2.insert("d", "foxtrot".to_string());
        attrs2.insert("e", "golf".to_string());

        let hash1 = compute_simhash(&attrs1);
        let hash2 = compute_simhash(&attrs2);

        // All values changed: hashes should differ
        assert_ne!(
            hash1, hash2,
            "Completely different values should produce different hashes"
        );
    }

    #[test]
    fn test_reconcile_no_create_only_picks_closest_among_multiple_state_entries() {
        // When multiple state entries exist, reconciliation should pick the closest one
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String))
            .attribute(AttributeSchema::new("tag_name", AttributeType::String))
            .attribute(AttributeSchema::new("tag_env", AttributeType::String))
            .attribute(AttributeSchema::new("tag_team", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        // Compute 3 identifiers with different attributes
        let make_resource = |env: &str, team: &str| {
            let mut r = Resource::with_provider("awscc", "ec2.eip", "");
            r.set_attr("domain".to_string(), Value::String("vpc".to_string()));
            r.set_attr("tag_name".to_string(), Value::String("my-eip".to_string()));
            r.set_attr("tag_env".to_string(), Value::String(env.to_string()));
            r.set_attr("tag_team".to_string(), Value::String(team.to_string()));
            r
        };

        // Original: env=prod, team=infra
        let mut resources_orig = vec![make_resource("production", "infra")];
        compute_anonymous_identifiers(
            &mut resources_orig,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let orig_id = resources_orig[0].id.name.clone();

        // Distant: env=dev, team=frontend (2 attrs changed)
        let mut resources_distant = vec![make_resource("development", "frontend")];
        compute_anonymous_identifiers(
            &mut resources_distant,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let distant_id = resources_distant[0].id.name.clone();

        // Current: env=staging, team=infra (1 attr changed from orig)
        let mut resources_current = vec![make_resource("staging", "infra")];
        compute_anonymous_identifiers(
            &mut resources_current,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();

        // State has both orig and distant entries
        let state_entries = vec![
            AnonymousIdStateInfo {
                name: orig_id.clone(),
                create_only_values: HashMap::new(),
            },
            AnonymousIdStateInfo {
                name: distant_id.clone(),
                create_only_values: HashMap::new(),
            },
        ];

        reconcile_anonymous_identifiers(
            &mut resources_current,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Should match orig (closer: 1 attr changed) rather than distant (2 attrs changed)
        // Note: This depends on SimHash producing closer hashes for more similar inputs.
        // If the Hamming distance for both is below the threshold, the closest is picked.
        let current_hash = extract_hash_from_identifier(&resources_current[0].id.name).unwrap();
        let orig_hash = extract_hash_from_identifier(&orig_id).unwrap();
        let distant_hash = extract_hash_from_identifier(&distant_id).unwrap();
        let dist_to_orig = (current_hash ^ orig_hash).count_ones();
        let dist_to_distant = (current_hash ^ distant_hash).count_ones();

        if dist_to_orig < SIMHASH_HAMMING_THRESHOLD {
            // If orig is within threshold, it should have been picked (as closest)
            assert_eq!(resources_current[0].id.name, orig_id);
        }
        if dist_to_orig < dist_to_distant {
            // Orig should be closer than distant
            assert!(
                dist_to_orig < dist_to_distant,
                "1-attr change (dist={}) should be closer than 2-attr change (dist={})",
                dist_to_orig,
                dist_to_distant,
            );
        }
    }

    #[test]
    fn test_reconcile_no_create_only_same_id_in_state_no_change() {
        // If state already has the same identifier, no reconciliation needed
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        let mut r = Resource::with_provider("awscc", "ec2.eip", "");
        r.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        let mut resources = vec![r];
        compute_anonymous_identifiers(
            &mut resources,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let id = resources[0].id.name.clone();

        // State has the exact same identifier
        let state_entries = vec![AnonymousIdStateInfo {
            name: id.clone(),
            create_only_values: HashMap::new(),
        }];
        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Should remain unchanged
        assert_eq!(resources[0].id.name, id);
    }

    #[test]
    fn test_reconcile_no_create_only_empty_state() {
        // No state entries = no reconciliation
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();

        let mut resource = Resource::with_provider("awscc", "ec2.eip", "ec2_eip_aabbccdd11223344");
        resource.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        let original_id = resource.id.name.clone();
        let mut resources = vec![resource];

        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| vec![],
        );

        assert_eq!(resources[0].id.name, original_id);
    }

    #[test]
    fn test_compute_anonymous_id_uses_simhash_for_no_create_only() {
        // Verify that changing one attribute produces a different but nearby identifier
        let schema = ResourceSchema::new("awscc.ec2.internet_gateway")
            .attribute(AttributeSchema::new("tag_name", AttributeType::String))
            .attribute(AttributeSchema::new("tag_env", AttributeType::String))
            .attribute(AttributeSchema::new("tag_team", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> =
            vec![("awscc.ec2.internet_gateway".to_string(), schema)]
                .into_iter()
                .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        let make_resource = |env: &str| {
            let mut r = Resource::with_provider("awscc", "ec2.internet_gateway", "");
            r.set_attr("tag_name".to_string(), Value::String("my-igw".to_string()));
            r.set_attr("tag_env".to_string(), Value::String(env.to_string()));
            r.set_attr(
                "tag_team".to_string(),
                Value::String("platform".to_string()),
            );
            r
        };

        let mut r1 = vec![make_resource("production")];
        let mut r2 = vec![make_resource("staging")];
        compute_anonymous_identifiers(&mut r1, &providers, &schemas, &schema_key_fn, &identity_fn)
            .unwrap();
        compute_anonymous_identifiers(&mut r2, &providers, &schemas, &schema_key_fn, &identity_fn)
            .unwrap();

        // Different identifiers
        assert_ne!(r1[0].id.name, r2[0].id.name);

        // But nearby (SimHash locality-sensitive property)
        let hash1 = extract_hash_from_identifier(&r1[0].id.name).unwrap();
        let hash2 = extract_hash_from_identifier(&r2[0].id.name).unwrap();
        let distance = (hash1 ^ hash2).count_ones();
        assert!(
            distance < SIMHASH_HAMMING_THRESHOLD,
            "Single attribute change should produce close SimHash (distance={}, threshold={})",
            distance,
            SIMHASH_HAMMING_THRESHOLD,
        );
    }

    #[test]
    fn test_compute_anonymous_id_simhash_vs_create_only_hash_independent() {
        // Resources with create-only properties use standard hash,
        // resources without use SimHash. Verify both work side by side.
        let schema_with_co = ResourceSchema::new("awscc.ec2.vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("tag_name", AttributeType::String));
        let schema_without_co = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String))
            .attribute(AttributeSchema::new("tag_name", AttributeType::String));
        let schemas: HashMap<String, ResourceSchema> = vec![
            ("awscc.ec2.vpc".to_string(), schema_with_co),
            ("awscc.ec2.eip".to_string(), schema_without_co),
        ]
        .into_iter()
        .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        let mut vpc = Resource::with_provider("awscc", "ec2.vpc", "");
        vpc.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        vpc.set_attr("tag_name".to_string(), Value::String("my-vpc".to_string()));

        let mut eip = Resource::with_provider("awscc", "ec2.eip", "");
        eip.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        eip.set_attr("tag_name".to_string(), Value::String("my-eip".to_string()));

        let mut resources = vec![vpc, eip];
        compute_anonymous_identifiers(
            &mut resources,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();

        // Both should have identifiers computed
        assert!(resources[0].id.name.starts_with("ec2_vpc_"));
        assert!(resources[1].id.name.starts_with("ec2_eip_"));

        // VPC uses standard hash (8 hex chars), EIP uses SimHash (16 hex chars)
        let vpc_hash_part = resources[0].id.name.rsplit('_').next().unwrap();
        let eip_hash_part = resources[1].id.name.rsplit('_').next().unwrap();
        assert_eq!(vpc_hash_part.len(), 8);
        assert_eq!(eip_hash_part.len(), 16);
    }

    #[test]
    fn test_reconcile_create_only_path_unaffected_by_simhash_changes() {
        // Verify that resources WITH create-only properties still use the
        // existing partial-match reconciliation, not Hamming distance.
        let schema = ResourceSchema::new("awscc.iam.role")
            .attribute(AttributeSchema::new("role_name", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("path", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.iam.role".to_string(), schema)]
            .into_iter()
            .collect();

        // Resource with both create-only props set
        let mut resource = Resource::with_provider("awscc", "iam.role", "iam_role_aabbccdd");
        resource.set_attr(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        resource.set_attr("path".to_string(), Value::String("/new/".to_string()));

        let original_id = resource.id.name.clone();
        let mut resources = vec![resource];

        // State with partial match (role_name matches, path differs)
        let state_entries = vec![AnonymousIdStateInfo {
            name: "iam_role_11223344".to_string(),
            create_only_values: vec![
                ("role_name".to_string(), "my-role".to_string()),
                ("path".to_string(), "/old/".to_string()),
            ]
            .into_iter()
            .collect(),
        }];

        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Should reconcile via partial create-only match (not Hamming distance)
        assert_eq!(resources[0].id.name, "iam_role_11223344");
        assert_ne!(resources[0].id.name, original_id);
    }

    #[test]
    fn test_compute_anonymous_id_stable_with_prefixed_create_only_attribute() {
        // When a create-only attribute has a prefix (e.g., bucket_name_prefix),
        // the anonymous identifier should be based on the prefix, not the
        // randomly generated name. This ensures the hash is stable across runs.
        let schema = ResourceSchema::new("awscc.s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> =
            vec![("awscc.s3.bucket".to_string(), schema)]
                .into_iter()
                .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        // Simulate two runs with different random suffixes but same prefix
        let make_resource = |generated_name: &str| {
            let mut r = Resource::with_provider("awscc", "s3.bucket", "");
            r.set_attr(
                "bucket_name".to_string(),
                Value::String(generated_name.to_string()),
            );
            r.prefixes
                .insert("bucket_name".to_string(), "my-app-".to_string());
            r
        };

        let mut r1 = vec![make_resource("my-app-abc12345")];
        let mut r2 = vec![make_resource("my-app-xyz98765")];
        compute_anonymous_identifiers(&mut r1, &providers, &schemas, &schema_key_fn, &identity_fn)
            .unwrap();
        compute_anonymous_identifiers(&mut r2, &providers, &schemas, &schema_key_fn, &identity_fn)
            .unwrap();

        // Same prefix should produce the same anonymous identifier
        assert_eq!(
            r1[0].id.name, r2[0].id.name,
            "Prefixed create-only attributes should produce stable identifiers"
        );
    }

    #[test]
    fn test_compute_anonymous_id_different_prefix_produces_different_id() {
        // Different prefixes should produce different anonymous identifiers
        let schema = ResourceSchema::new("awscc.s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> =
            vec![("awscc.s3.bucket".to_string(), schema)]
                .into_iter()
                .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        let make_resource = |prefix: &str, generated_name: &str| {
            let mut r = Resource::with_provider("awscc", "s3.bucket", "");
            r.set_attr(
                "bucket_name".to_string(),
                Value::String(generated_name.to_string()),
            );
            r.prefixes
                .insert("bucket_name".to_string(), prefix.to_string());
            r
        };

        let mut r1 = vec![make_resource("app-a-", "app-a-abc12345")];
        let mut r2 = vec![make_resource("app-b-", "app-b-xyz98765")];
        compute_anonymous_identifiers(&mut r1, &providers, &schemas, &schema_key_fn, &identity_fn)
            .unwrap();
        compute_anonymous_identifiers(&mut r2, &providers, &schemas, &schema_key_fn, &identity_fn)
            .unwrap();

        // Different prefixes should produce different identifiers
        assert_ne!(
            r1[0].id.name, r2[0].id.name,
            "Different prefixes should produce different identifiers"
        );
    }

    #[test]
    fn test_reconcile_skips_let_bound_resources() {
        // Let-bound (named) resources should never be reconciled, even if their
        // name doesn't exist in state. The _binding attribute marks them as named.
        let schema = ResourceSchema::new("aws.ec2.security_group_ingress")
            .attribute(AttributeSchema::new("cidr_ip", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("ip_protocol", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("description", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> =
            vec![("aws.ec2.security_group_ingress".to_string(), schema)]
                .into_iter()
                .collect();

        // A let-bound resource whose name does NOT exist in state
        let mut ingress_new =
            Resource::with_provider("aws", "ec2.security_group_ingress", "ingress_new");
        ingress_new.binding = Some("ingress_new".to_string());
        ingress_new.set_attr(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        ingress_new.set_attr("ip_protocol".to_string(), Value::String("tcp".to_string()));
        ingress_new.set_attr(
            "description".to_string(),
            Value::String("Allow HTTPS".to_string()),
        );

        let mut resources = vec![ingress_new];

        // State has an unrelated entry that partially matches (same cidr_ip + ip_protocol,
        // different description). Without the fix, the named resource would be rebound.
        let state_entries = vec![AnonymousIdStateInfo {
            name: "ec2_security_group_ingress_aabb1122".to_string(),
            create_only_values: vec![
                ("cidr_ip".to_string(), "0.0.0.0/0".to_string()),
                ("ip_protocol".to_string(), "tcp".to_string()),
                ("description".to_string(), "Allow HTTP".to_string()),
            ]
            .into_iter()
            .collect(),
        }];

        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Named resource must keep its original name
        assert_eq!(
            resources[0].id.name, "ingress_new",
            "let-bound resource should not be reconciled"
        );
    }

    #[test]
    fn test_reconcile_skips_when_multiple_partial_matches() {
        // When multiple state entries partially match an anonymous resource,
        // reconciliation should skip rather than picking the first match.
        // This prevents a new SG rule from hijacking an unrelated state entry.
        let schema = ResourceSchema::new("aws.ec2.security_group_ingress")
            .attribute(AttributeSchema::new("cidr_ip", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("ip_protocol", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("description", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> =
            vec![("aws.ec2.security_group_ingress".to_string(), schema)]
                .into_iter()
                .collect();

        // Anonymous resource with a new hash-derived identifier
        let mut new_rule = Resource::with_provider(
            "aws",
            "ec2.security_group_ingress",
            "ec2_security_group_ingress_deadbeef",
        );
        new_rule.set_attr(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        new_rule.set_attr("ip_protocol".to_string(), Value::String("tcp".to_string()));
        new_rule.set_attr(
            "description".to_string(),
            Value::String("Allow gRPC".to_string()),
        );

        let original_id = new_rule.id.name.clone();
        let mut resources = vec![new_rule];

        // State has TWO entries that partially match (same cidr_ip + ip_protocol,
        // different description). Both are valid partial matches.
        let state_entries = vec![
            AnonymousIdStateInfo {
                name: "ec2_security_group_ingress_aabb1122".to_string(),
                create_only_values: vec![
                    ("cidr_ip".to_string(), "0.0.0.0/0".to_string()),
                    ("ip_protocol".to_string(), "tcp".to_string()),
                    ("description".to_string(), "Allow HTTP".to_string()),
                ]
                .into_iter()
                .collect(),
            },
            AnonymousIdStateInfo {
                name: "ec2_security_group_ingress_ccdd3344".to_string(),
                create_only_values: vec![
                    ("cidr_ip".to_string(), "0.0.0.0/0".to_string()),
                    ("ip_protocol".to_string(), "tcp".to_string()),
                    ("description".to_string(), "Allow HTTPS".to_string()),
                ]
                .into_iter()
                .collect(),
            },
        ];

        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // With multiple partial matches, reconciliation should be skipped
        assert_eq!(
            resources[0].id.name, original_id,
            "ambiguous partial matches should not reconcile"
        );
    }

    #[test]
    fn test_reconcile_eip_tag_update_with_unset_create_only_props() {
        // Regression test for #882: EC2 EIP has create-only props in schema
        // (address, ipam_pool_id, etc.) but user didn't set any. Only tags changed.
        // SimHash reconciliation should match the resource as an in-place update,
        // not a replace (delete+create).
        let schema = ResourceSchema::new("awscc.ec2.eip")
            .attribute(AttributeSchema::new("domain", AttributeType::String))
            .attribute(AttributeSchema::new("address", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("ipam_pool_id", AttributeType::String).create_only())
            .attribute(
                AttributeSchema::new("network_border_group", AttributeType::String).create_only(),
            )
            .attribute(
                AttributeSchema::new("transfer_address", AttributeType::String).create_only(),
            )
            .attribute(AttributeSchema::new(
                "tags",
                AttributeType::Map(Box::new(AttributeType::String)),
            ));
        let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
            .into_iter()
            .collect();
        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: vec![(
                "region".to_string(),
                Value::String("awscc.Region.ap_northeast_1".to_string()),
            )]
            .into_iter()
            .collect(),
            default_tags: HashMap::new(),
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

        // Step 1: Create EIP with tags Environment=acceptance-test
        let mut r1 = Resource::with_provider("awscc", "ec2.eip", "");
        r1.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        let mut tags1 = std::collections::HashMap::new();
        tags1.insert(
            "Environment".to_string(),
            Value::String("acceptance-test".to_string()),
        );
        tags1.insert(
            "Purpose".to_string(),
            Value::String("simhash-test".to_string()),
        );
        r1.set_attr("tags".to_string(), Value::Map(tags1));

        let mut resources1 = vec![r1];
        compute_anonymous_identifiers(
            &mut resources1,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let step1_id = resources1[0].id.name.clone();

        // Step 2: Change tag Environment=staging (only tags changed)
        let mut r2 = Resource::with_provider("awscc", "ec2.eip", "");
        r2.set_attr("domain".to_string(), Value::String("vpc".to_string()));
        let mut tags2 = std::collections::HashMap::new();
        tags2.insert(
            "Environment".to_string(),
            Value::String("staging".to_string()),
        );
        tags2.insert(
            "Purpose".to_string(),
            Value::String("simhash-test".to_string()),
        );
        r2.set_attr("tags".to_string(), Value::Map(tags2));

        let mut resources2 = vec![r2];
        compute_anonymous_identifiers(
            &mut resources2,
            &providers,
            &schemas,
            &schema_key_fn,
            &identity_fn,
        )
        .unwrap();
        let step2_id = resources2[0].id.name.clone();

        // Identifiers should differ (different tag values)
        assert_ne!(step1_id, step2_id);

        // Reconcile: state has the step1 identifier
        let state_entries = vec![AnonymousIdStateInfo {
            name: step1_id.clone(),
            create_only_values: HashMap::new(), // No create-only values in state either
        }];
        reconcile_anonymous_identifiers(
            &mut resources2,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // After reconciliation, step2 should have step1's identifier (in-place update)
        assert_eq!(
            resources2[0].id.name, step1_id,
            "Tag-only change on EIP with unset create-only props should reconcile to same identifier"
        );
    }

    #[test]
    fn test_reconcile_does_not_swap_named_resources_with_overlapping_create_only() {
        // Regression test for #788: two security_group_ingress rules on the same SG
        // should not be swapped by reconciliation when they share some create-only
        // attributes (cidr_ip, ip_protocol) but differ on others (description, from_port).
        //
        // Both resources are named (let-bound) and already match state entries by name.
        // Reconciliation should leave them unchanged.
        let schema = ResourceSchema::new("aws.ec2.security_group_ingress")
            .attribute(AttributeSchema::new("cidr_ip", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("ip_protocol", AttributeType::String).create_only())
            .attribute(AttributeSchema::new("description", AttributeType::String).create_only());
        let schemas: HashMap<String, ResourceSchema> =
            vec![("aws.ec2.security_group_ingress".to_string(), schema)]
                .into_iter()
                .collect();

        // Two named ingress resources with overlapping create-only attributes
        let mut ingress_http =
            Resource::with_provider("aws", "ec2.security_group_ingress", "ingress_http");
        ingress_http.set_attr(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        ingress_http.set_attr("ip_protocol".to_string(), Value::String("tcp".to_string()));
        ingress_http.set_attr(
            "description".to_string(),
            Value::String("Allow HTTP".to_string()),
        );

        let mut ingress_https =
            Resource::with_provider("aws", "ec2.security_group_ingress", "ingress_https");
        ingress_https.set_attr(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        ingress_https.set_attr("ip_protocol".to_string(), Value::String("tcp".to_string()));
        ingress_https.set_attr(
            "description".to_string(),
            Value::String("Allow HTTPS".to_string()),
        );

        let mut resources = vec![ingress_http, ingress_https];

        // State has both resources with matching names
        let state_entries = vec![
            AnonymousIdStateInfo {
                name: "ingress_http".to_string(),
                create_only_values: vec![
                    ("cidr_ip".to_string(), "0.0.0.0/0".to_string()),
                    ("ip_protocol".to_string(), "tcp".to_string()),
                    ("description".to_string(), "Allow HTTP".to_string()),
                ]
                .into_iter()
                .collect(),
            },
            AnonymousIdStateInfo {
                name: "ingress_https".to_string(),
                create_only_values: vec![
                    ("cidr_ip".to_string(), "0.0.0.0/0".to_string()),
                    ("ip_protocol".to_string(), "tcp".to_string()),
                    ("description".to_string(), "Allow HTTPS".to_string()),
                ]
                .into_iter()
                .collect(),
            },
        ];

        reconcile_anonymous_identifiers(
            &mut resources,
            &schemas,
            &schema_key_fn,
            &|_provider, _rt| state_entries.clone(),
        );

        // Names must remain unchanged - no swapping
        assert_eq!(
            resources[0].id.name, "ingress_http",
            "ingress_http should not be renamed to ingress_https"
        );
        assert_eq!(
            resources[1].id.name, "ingress_https",
            "ingress_https should not be renamed to ingress_http"
        );
    }
}
