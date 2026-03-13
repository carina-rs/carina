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
                    && let Value::String(prefix_value) = value
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
            resource
                .attributes
                .insert(base_attr, Value::String(generated_name));
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
    find_state: &dyn Fn(&str, &str) -> Option<PrefixStateInfo>,
) {
    for resource in resources.iter_mut() {
        if resource.prefixes.is_empty() {
            continue;
        }

        // Find matching resource in state
        let state_info = find_state(&resource.id.resource_type, &resource.id.name);
        let state_info = match state_info {
            Some(si) => si,
            None => continue,
        };

        for (attr_name, prefix) in &resource.prefixes {
            // Check if state has the same prefix for this attribute
            if let Some(state_prefix) = state_info.prefixes.get(attr_name)
                && state_prefix == prefix
            {
                // Same prefix: reuse the existing name from state
                if let Some(name_str) = state_info.attribute_values.get(attr_name) {
                    resource
                        .attributes
                        .insert(attr_name.clone(), Value::String(name_str.clone()));
                }
            }
            // If prefix changed or no state prefix exists, keep the newly generated name
        }
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

        // Look up schema for this resource's provider (skip if no _provider set)
        let Some(Value::String(provider_name)) = resource.attributes.get("_provider") else {
            continue;
        };
        let schema_key = schema_key_fn(resource);

        let Some(schema) = schemas.get(&schema_key) else {
            continue;
        };

        let create_only_attrs = schema.create_only_attributes();
        if create_only_attrs.is_empty() {
            return Err(format!(
                "Anonymous resource '{}' has no create-only properties. Use `let` binding for identification.",
                resource.id.display_type()
            ));
        }

        // Collect identity attribute values (e.g., region) from provider config
        let mut identity_values: BTreeMap<String, String> = BTreeMap::new();
        let identity_attrs = identity_attributes_fn(provider_name);
        if let Some(pc) = providers.iter().find(|p| p.name == *provider_name) {
            for attr_name in &identity_attrs {
                if let Some(value) = pc.attributes.get(attr_name.as_str()) {
                    identity_values.insert(attr_name.clone(), format!("{:?}", value));
                }
            }
        }

        // Collect create-only values in sorted order for deterministic hashing
        let mut create_only_values: BTreeMap<&str, String> = BTreeMap::new();
        for attr_name in &create_only_attrs {
            if let Some(value) = resource.attributes.get(*attr_name) {
                create_only_values.insert(attr_name, format!("{:?}", value));
            }
        }

        if create_only_values.is_empty() {
            return Err(format!(
                "Anonymous resource '{}' has no create-only property values set. Use `let` binding for identification.",
                resource.id.display_type()
            ));
        }

        // Compute deterministic hash: identity attributes first, then create-only values
        let mut hasher = std::hash::DefaultHasher::new();
        for (k, v) in &identity_values {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }
        for (k, v) in &create_only_values {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }
        let hash = hasher.finish();
        let hash_str = format!("{:08x}", hash & 0xFFFFFFFF); // 8 hex chars

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

        let schema_key = schema_key_fn(resource);
        let Some(schema) = schemas.get(&schema_key) else {
            continue;
        };

        let create_only_attrs = schema.create_only_attributes();
        if create_only_attrs.is_empty() {
            continue;
        }

        // Collect this resource's create-only values
        let mut resource_co_values: HashMap<&str, String> = HashMap::new();
        for attr_name in &create_only_attrs {
            if let Some(Value::String(v)) = resource.attributes.get(*attr_name) {
                resource_co_values.insert(attr_name, v.clone());
            }
        }
        if resource_co_values.is_empty() {
            continue;
        }

        let state_entries = find_state_by_type(&resource.id.provider, &resource.id.resource_type);
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

            // Reconcile if at least one create-only property matches and
            // at least one differs (partial match = same resource with changes)
            if matched > 0 && mismatched > 0 {
                resource.id = ResourceId::with_provider(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    &entry.name,
                );
                break;
            }
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
        match resource.attributes.get("_provider") {
            Some(Value::String(provider)) => format!("{}.{}", provider, resource.id.resource_type),
            _ => resource.id.resource_type.clone(),
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
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
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
        let bucket_name = match resources[0].attributes.get("bucket_name").unwrap() {
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
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
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
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "bucket_name_prefix".to_string(),
            Value::String("my-app-".to_string()),
        );
        resource.attributes.insert(
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
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
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
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("my-app-temporary".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &|_resource_type, _name| {
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
            resources[0].attributes.get("bucket_name"),
            Some(&Value::String("my-app-existing1".to_string()))
        );
    }

    #[test]
    fn test_reconcile_prefixed_names_generates_new_name_when_prefix_changes() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "new-prefix-".to_string());
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("new-prefix-abcd1234".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &|_resource_type, _name| {
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
            resources[0].attributes.get("bucket_name"),
            Some(&Value::String("new-prefix-abcd1234".to_string()))
        );
    }

    #[test]
    fn test_reconcile_prefixed_names_keeps_generated_name_when_no_state() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("my-app-abcd1234".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &|_resource_type, _name| None);

        // No state, so keep the generated name
        assert_eq!(
            resources[0].attributes.get("bucket_name"),
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
        }];
        let identity_fn = |_: &str| -> Vec<String> { vec![] };

        // Step 1: compute identifier with path="/"
        let mut r1 = Resource::with_provider("awscc", "iam.role", "");
        r1.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r1.attributes.insert(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        r1.attributes
            .insert("path".to_string(), Value::String("/".to_string()));
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
        r2.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r2.attributes.insert(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        r2.attributes
            .insert("path".to_string(), Value::String("/carina/".to_string()));
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
        resource.attributes.insert(
            "role_name".to_string(),
            Value::String("new-role".to_string()),
        );
        resource
            .attributes
            .insert("path".to_string(), Value::String("/new/".to_string()));

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
        resource.attributes.insert(
            "role_name".to_string(),
            Value::String("my-role".to_string()),
        );
        resource
            .attributes
            .insert("path".to_string(), Value::String("/".to_string()));

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
        resource.attributes.insert(
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
}
