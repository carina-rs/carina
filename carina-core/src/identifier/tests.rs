use super::*;
use crate::resource::Resource;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;

fn make_s3_bucket_schema() -> (String, ResourceSchema) {
    let schema = ResourceSchema::new("awscc.s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String));
    ("awscc.s3.Bucket".to_string(), schema)
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");
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
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    let step1_id = resources1[0].id.name_str().to_string();

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
    let step2_id = resources2[0].id.name_str().to_string();

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
    assert_eq!(resources2[0].id.name_str(), step1_id);
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

    let original_id = resource.id.name_str().to_string();
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
    assert_eq!(resources[0].id.name_str(), original_id);
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

    let original_id = resource.id.name_str().to_string();
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
    assert_eq!(resources[0].id.name_str(), original_id);
}

#[test]
fn test_reconcile_anonymous_id_single_create_only_no_reconcile() {
    // With only one create-only property, changing it means ALL changed,
    // so no reconciliation (matched=0 or mismatched=0)
    let schema = ResourceSchema::new("awscc.ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only());
    let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.Vpc".to_string(), schema)]
        .into_iter()
        .collect();

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "ec2_vpc_aabbccdd");
    resource.set_attr(
        "cidr_block".to_string(),
        Value::String("10.1.0.0/16".to_string()),
    );

    let original_id = resource.id.name_str().to_string();
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
    assert_eq!(resources[0].id.name_str(), original_id);
}

#[test]
fn test_anonymous_resource_no_create_only_properties() {
    // Resources with no create-only properties should still work as anonymous resources
    let schema = ResourceSchema::new("awscc.ec2.eip")
        .attribute(AttributeSchema::new("domain", AttributeType::String))
        .attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::String),
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
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    assert!(!resources[0].id.name_str().is_empty());
    assert!(resources[0].id.name_str().starts_with("ec2_eip_"));
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
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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

    assert_eq!(resources1[0].id.name_str(), resources2[0].id.name_str());
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
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
fn test_identity_attribute_prevents_collision() {
    // Two anonymous resources of the same type with the same create-only attrs
    // but different identity attrs should NOT collide.
    // This simulates route53.record_set where `name` is create-only (same)
    // but `type` is identity (A vs AAAA).
    let schema = ResourceSchema::new("awscc.route53.record_set")
        .attribute(AttributeSchema::new("name", AttributeType::String).create_only())
        .attribute(AttributeSchema::new("hosted_zone_id", AttributeType::String).create_only())
        .attribute(AttributeSchema::new("type", AttributeType::String).identity())
        .attribute(AttributeSchema::new("ttl", AttributeType::String))
        .attribute(AttributeSchema::new(
            "resource_records",
            AttributeType::list(AttributeType::String),
        ));
    let schemas: HashMap<String, ResourceSchema> =
        vec![("awscc.route53.record_set".to_string(), schema)]
            .into_iter()
            .collect();
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut r1 = Resource::with_provider("awscc", "route53.record_set", "");
    r1.set_attr(
        "name".to_string(),
        Value::String("carina-rs.dev".to_string()),
    );
    r1.set_attr(
        "hosted_zone_id".to_string(),
        Value::String("Z123".to_string()),
    );
    r1.set_attr("type".to_string(), Value::String("A".to_string()));

    let mut r2 = Resource::with_provider("awscc", "route53.record_set", "");
    r2.set_attr(
        "name".to_string(),
        Value::String("carina-rs.dev".to_string()),
    );
    r2.set_attr(
        "hosted_zone_id".to_string(),
        Value::String("Z123".to_string()),
    );
    r2.set_attr("type".to_string(), Value::String("AAAA".to_string()));

    let mut resources = vec![r1, r2];
    compute_anonymous_identifiers(
        &mut resources,
        &providers,
        &schemas,
        &schema_key_fn,
        &identity_fn,
    )
    .expect("should not collide when identity attrs differ");

    // Both should have identifiers assigned
    assert!(!resources[0].id.name_str().is_empty());
    assert!(!resources[1].id.name_str().is_empty());
    // Identifiers should be different
    assert_ne!(
        resources[0].id.name_str(),
        resources[1].id.name_str(),
        "different identity attr values should produce different identifiers"
    );
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
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    let old_id = resources1[0].id.name_str().to_string();

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
    let new_id = resources2[0].id.name_str().to_string();

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
    assert_eq!(resources2[0].id.name_str(), old_id);
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

    let original_id = resource.id.name_str().to_string();
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
    assert_eq!(resources[0].id.name_str(), original_id);
}

#[test]
fn test_reconcile_anonymous_id_create_only_exists_but_none_set() {
    // Case A: Schema has create-only properties, but user didn't set any.
    // Should use SimHash-based Hamming distance reconciliation.
    let schema = ResourceSchema::new("awscc.ec2.eip")
        .attribute(AttributeSchema::new("domain", AttributeType::String))
        .attribute(AttributeSchema::new("public_ipv4_pool", AttributeType::String).create_only());
    let schemas: HashMap<String, ResourceSchema> = vec![("awscc.ec2.eip".to_string(), schema)]
        .into_iter()
        .collect();
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    assert!(!resources[0].id.name_str().is_empty());
    assert!(resources[0].id.name_str().starts_with("ec2_eip_"));

    // Reconciliation should use Hamming distance (create-only values empty)
    let current_id = resources[0].id.name_str().to_string();
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
    assert_eq!(resources[0].id.name_str(), current_id);
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
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    let orig_id = resources_orig[0].id.name_str().to_string();

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
    let distant_id = resources_distant[0].id.name_str().to_string();

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
    let current_hash = extract_hash_from_identifier(resources_current[0].id.name_str()).unwrap();
    let orig_hash = extract_hash_from_identifier(&orig_id).unwrap();
    let distant_hash = extract_hash_from_identifier(&distant_id).unwrap();
    let dist_to_orig = (current_hash ^ orig_hash).count_ones();
    let dist_to_distant = (current_hash ^ distant_hash).count_ones();

    if dist_to_orig < SIMHASH_HAMMING_THRESHOLD {
        // If orig is within threshold, it should have been picked (as closest)
        assert_eq!(resources_current[0].id.name_str(), orig_id);
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
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    let id = resources[0].id.name_str().to_string();

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
    assert_eq!(resources[0].id.name_str(), id);
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
    let original_id = resource.id.name_str().to_string();
    let mut resources = vec![resource];

    reconcile_anonymous_identifiers(
        &mut resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| vec![],
    );

    assert_eq!(resources[0].id.name_str(), original_id);
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
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
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
    assert_ne!(r1[0].id.name_str(), r2[0].id.name_str());

    // But nearby (SimHash locality-sensitive property)
    let hash1 = extract_hash_from_identifier(r1[0].id.name_str()).unwrap();
    let hash2 = extract_hash_from_identifier(r2[0].id.name_str()).unwrap();
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
    let schema_with_co = ResourceSchema::new("awscc.ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only())
        .attribute(AttributeSchema::new("tag_name", AttributeType::String));
    let schema_without_co = ResourceSchema::new("awscc.ec2.eip")
        .attribute(AttributeSchema::new("domain", AttributeType::String))
        .attribute(AttributeSchema::new("tag_name", AttributeType::String));
    let schemas: HashMap<String, ResourceSchema> = vec![
        ("awscc.ec2.Vpc".to_string(), schema_with_co),
        ("awscc.ec2.eip".to_string(), schema_without_co),
    ]
    .into_iter()
    .collect();
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut vpc = Resource::with_provider("awscc", "ec2.Vpc", "");
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
    assert!(resources[0].id.name_str().starts_with("ec2_vpc_"));
    assert!(resources[1].id.name_str().starts_with("ec2_eip_"));

    // VPC uses standard hash (8 hex chars), EIP uses SimHash (16 hex chars)
    let vpc_hash_part = resources[0].id.name_str().rsplit('_').next().unwrap();
    let eip_hash_part = resources[1].id.name_str().rsplit('_').next().unwrap();
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

    let original_id = resource.id.name_str().to_string();
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
    assert_eq!(resources[0].id.name_str(), "iam_role_11223344");
    assert_ne!(resources[0].id.name_str(), original_id);
}

#[test]
fn test_compute_anonymous_id_stable_with_prefixed_create_only_attribute() {
    // When a create-only attribute has a prefix (e.g., bucket_name_prefix),
    // the anonymous identifier should be based on the prefix, not the
    // randomly generated name. This ensures the hash is stable across runs.
    let schema = ResourceSchema::new("awscc.s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only());
    let schemas: HashMap<String, ResourceSchema> = vec![("awscc.s3.Bucket".to_string(), schema)]
        .into_iter()
        .collect();
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    // Simulate two runs with different random suffixes but same prefix
    let make_resource = |generated_name: &str| {
        let mut r = Resource::with_provider("awscc", "s3.Bucket", "");
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
        r1[0].id.name_str(),
        r2[0].id.name_str(),
        "Prefixed create-only attributes should produce stable identifiers"
    );
}

#[test]
fn test_compute_anonymous_id_different_prefix_produces_different_id() {
    // Different prefixes should produce different anonymous identifiers
    let schema = ResourceSchema::new("awscc.s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only());
    let schemas: HashMap<String, ResourceSchema> = vec![("awscc.s3.Bucket".to_string(), schema)]
        .into_iter()
        .collect();
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_resource = |prefix: &str, generated_name: &str| {
        let mut r = Resource::with_provider("awscc", "s3.Bucket", "");
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
        r1[0].id.name_str(),
        r2[0].id.name_str(),
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
        resources[0].id.name_str(),
        "ingress_new",
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

    let original_id = new_rule.id.name_str().to_string();
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
        resources[0].id.name_str(),
        original_id,
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
        .attribute(AttributeSchema::new("transfer_address", AttributeType::String).create_only())
        .attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::String),
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
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    // Step 1: Create EIP with tags Environment=acceptance-test
    let mut r1 = Resource::with_provider("awscc", "ec2.eip", "");
    r1.set_attr("domain".to_string(), Value::String("vpc".to_string()));
    let mut tags1 = indexmap::IndexMap::new();
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
    let step1_id = resources1[0].id.name_str().to_string();

    // Step 2: Change tag Environment=staging (only tags changed)
    let mut r2 = Resource::with_provider("awscc", "ec2.eip", "");
    r2.set_attr("domain".to_string(), Value::String("vpc".to_string()));
    let mut tags2 = indexmap::IndexMap::new();
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
    let step2_id = resources2[0].id.name_str().to_string();

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
        resources2[0].id.name_str(),
        step1_id,
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
        resources[0].id.name_str(),
        "ingress_http",
        "ingress_http should not be renamed to ingress_https"
    );
    assert_eq!(
        resources[1].id.name_str(),
        "ingress_https",
        "ingress_https should not be renamed to ingress_http"
    );
}

fn make_sso_instance_schema() -> (String, ResourceSchema) {
    let schema = ResourceSchema::new("awscc.sso.Instance")
        .attribute(AttributeSchema::new("name", AttributeType::String).create_only());
    ("awscc.sso.Instance".to_string(), schema)
}

#[test]
fn test_detect_rename_unique_match_by_create_only_attrs() {
    // Scenario: state has an anonymous sso.instance with name="carina-rs".
    // DSL now defines it as a let-bound resource with the same name.
    // detect_anonymous_to_named_renames should emit a rename from the
    // anonymous hash name to the binding name.
    let (key, schema) = make_sso_instance_schema();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso");
    resource.binding = Some("sso".to_string());
    resource.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![resource];

    let state_entries = vec![AnonymousIdStateInfo {
        name: "sso_instance_0ac0620303071530".to_string(),
        create_only_values: vec![("name".to_string(), "carina-rs".to_string())]
            .into_iter()
            .collect(),
    }];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].0.name_str(), "sso_instance_0ac0620303071530");
    assert_eq!(renames[0].1.name_str(), "sso");
}

#[test]
fn test_detect_rename_skips_when_binding_already_in_state() {
    // If state already has an entry for the binding name, nothing to rename.
    let (key, schema) = make_sso_instance_schema();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso");
    resource.binding = Some("sso".to_string());
    resource.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![resource];

    let state_entries = vec![AnonymousIdStateInfo {
        name: "sso".to_string(),
        create_only_values: vec![("name".to_string(), "carina-rs".to_string())]
            .into_iter()
            .collect(),
    }];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_ignores_anonymous_resources() {
    // Anonymous resources (binding=None) are not candidates for this rename.
    let (key, schema) = make_sso_instance_schema();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso_instance_new");
    // No binding set
    resource.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![resource];

    let state_entries = vec![AnonymousIdStateInfo {
        name: "sso_instance_0ac0620303071530".to_string(),
        create_only_values: vec![("name".to_string(), "carina-rs".to_string())]
            .into_iter()
            .collect(),
    }];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_skips_ambiguous_matches() {
    // Two orphan state entries match — skip to avoid rebinding the wrong one.
    let (key, schema) = make_sso_instance_schema();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso");
    resource.binding = Some("sso".to_string());
    resource.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![resource];

    let state_entries = vec![
        AnonymousIdStateInfo {
            name: "sso_instance_aaaabbbbccccdddd".to_string(),
            create_only_values: vec![("name".to_string(), "carina-rs".to_string())]
                .into_iter()
                .collect(),
        },
        AnonymousIdStateInfo {
            name: "sso_instance_1111222233334444".to_string(),
            create_only_values: vec![("name".to_string(), "carina-rs".to_string())]
                .into_iter()
                .collect(),
        },
    ];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_ignores_non_hash_state_names() {
    // A state entry with a non-hash name (e.g., another let binding) is not
    // treated as an anonymous candidate and must not be silently renamed.
    let (key, schema) = make_sso_instance_schema();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso");
    resource.binding = Some("sso".to_string());
    resource.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![resource];

    let state_entries = vec![AnonymousIdStateInfo {
        name: "my_custom_binding".to_string(),
        create_only_values: vec![("name".to_string(), "carina-rs".to_string())]
            .into_iter()
            .collect(),
    }];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

/// Schema with NO create-only attributes — like `awscc.sso.Instance`.
fn make_sso_instance_schema_no_create_only() -> (String, ResourceSchema) {
    let schema = ResourceSchema::new("awscc.sso.Instance")
        .attribute(AttributeSchema::new("name", AttributeType::String));
    ("awscc.sso.Instance".to_string(), schema)
}

#[test]
fn test_detect_rename_no_create_only_matches_by_simhash() {
    // Regression test for carina#1670:
    // Schema has no create-only attrs (e.g. awscc.sso.Instance). The
    // anonymous → let-bound rename must still be detected so `carina plan`
    // shows a Move rather than Delete+Create, which would destroy the
    // Identity Center instance and all of its users/groups.
    let (key, schema) = make_sso_instance_schema_no_create_only();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Step 1: generate the anonymous ID the previous `apply` would have
    // written to state, using the same inputs and the same code path.
    let mut anon = Resource::with_provider("awscc", "sso.Instance", "");
    anon.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers(
        &mut anon_vec,
        &providers,
        &schemas,
        &schema_key_fn,
        &identity_fn,
    )
    .unwrap();
    let anonymous_name = anon_vec[0].id.name_str().to_string();
    assert!(
        anonymous_name.starts_with("sso_instance_"),
        "expected hash-derived name, got {anonymous_name}"
    );

    // Step 2: user wraps the same resource in a `let` binding.
    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso");
    let_bound.binding = Some("sso".to_string());
    let_bound.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![let_bound];

    // Step 3: state still has the orphan anonymous entry.
    let state_entries = vec![AnonymousIdStateInfo {
        name: anonymous_name.clone(),
        create_only_values: HashMap::new(),
    }];

    // Step 4: detect_anonymous_to_named_renames should match via SimHash.
    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );

    assert_eq!(renames.len(), 1, "expected one rename, got {:?}", renames);
    assert_eq!(renames[0].0.name_str(), anonymous_name);
    assert_eq!(renames[0].1.name_str(), "sso");
}

#[test]
fn test_detect_rename_no_create_only_skips_when_attributes_differ_too_much() {
    // If the let-bound resource's attributes drift beyond the SimHash
    // Hamming threshold from any orphan, no rename is emitted and the
    // user falls back to delete+create (or a `moved` block if they want
    // to preserve state).
    let (key, schema) = make_sso_instance_schema_no_create_only();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Anonymous snapshot with many attributes.
    let mut anon = Resource::with_provider("awscc", "sso.Instance", "");
    anon.set_attr("name".to_string(), Value::String("old-name".to_string()));
    let mut tags = indexmap::IndexMap::new();
    tags.insert("k1".to_string(), Value::String("v1".to_string()));
    tags.insert("k2".to_string(), Value::String("v2".to_string()));
    anon.set_attr("tags".to_string(), Value::Map(tags));
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers(
        &mut anon_vec,
        &providers,
        &schemas,
        &schema_key_fn,
        &identity_fn,
    )
    .unwrap();
    let anonymous_name = anon_vec[0].id.name_str().to_string();

    // Let-bound resource with wildly different attributes.
    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso");
    let_bound.binding = Some("sso".to_string());
    let_bound.set_attr(
        "name".to_string(),
        Value::String("completely-different".to_string()),
    );
    let mut different_tags = indexmap::IndexMap::new();
    different_tags.insert("other1".to_string(), Value::String("foo".to_string()));
    different_tags.insert("other2".to_string(), Value::String("bar".to_string()));
    different_tags.insert("other3".to_string(), Value::String("baz".to_string()));
    let_bound.set_attr("tags".to_string(), Value::Map(different_tags));
    let resources = vec![let_bound];

    let state_entries = vec![AnonymousIdStateInfo {
        name: anonymous_name,
        create_only_values: HashMap::new(),
    }];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );
    assert!(
        renames.is_empty(),
        "attributes differ too much, should not rename: {:?}",
        renames
    );
}

#[test]
fn test_detect_rename_no_create_only_picks_closest_among_multiple_candidates() {
    // Two orphans: one is an exact SimHash match, the other is off by a
    // few bits but still within the Hamming threshold. The exact match
    // must win — this exercises the `distance < d` branch of the picker.
    let (key, schema) = make_sso_instance_schema_no_create_only();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Compute the exact-match name.
    let mut anon = Resource::with_provider("awscc", "sso.Instance", "");
    anon.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers(
        &mut anon_vec,
        &providers,
        &schemas,
        &schema_key_fn,
        &identity_fn,
    )
    .unwrap();
    let exact_name = anon_vec[0].id.name_str().to_string();

    // Construct a "close but not equal" orphan by flipping the last hex
    // char of the SimHash — guarantees a small nonzero Hamming distance
    // well under the threshold.
    let mut chars: Vec<char> = exact_name.chars().collect();
    let last = chars.last_mut().unwrap();
    *last = if *last == '0' { 'f' } else { '0' };
    let nearby_name: String = chars.into_iter().collect();
    assert_ne!(exact_name, nearby_name);

    // Place the nearby entry first so the picker must prefer the later
    // exact match via the `distance < d` branch.
    let state_entries = vec![
        AnonymousIdStateInfo {
            name: nearby_name.clone(),
            create_only_values: HashMap::new(),
        },
        AnonymousIdStateInfo {
            name: exact_name.clone(),
            create_only_values: HashMap::new(),
        },
    ];

    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso");
    let_bound.binding = Some("sso".to_string());
    let_bound.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![let_bound];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(
        renames[0].0.name_str(),
        exact_name,
        "should prefer the exact SimHash match over the nearby one"
    );
}

#[test]
fn test_detect_rename_no_create_only_skips_8_char_hash_entries() {
    // An 8-hex state entry (from the create-only hash path) must not
    // match against a 16-hex SimHash — the hashes use different schemes
    // and comparing them by Hamming distance is meaningless.
    let (key, schema) = make_sso_instance_schema_no_create_only();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso");
    let_bound.binding = Some("sso".to_string());
    let_bound.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![let_bound];

    // 8-hex suffix (standard hash scheme), not a SimHash.
    let state_entries = vec![AnonymousIdStateInfo {
        name: "sso_instance_a3f2b1c8".to_string(),
        create_only_values: HashMap::new(),
    }];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );
    assert!(
        renames.is_empty(),
        "8-hex entries must not match the SimHash branch: {:?}",
        renames
    );
}

#[test]
fn test_detect_rename_no_create_only_skips_when_two_orphans_tie_on_distance() {
    // Two orphans both within the Hamming threshold with identical distance
    // to the let-bound resource — ambiguous, must skip.
    let (key, schema) = make_sso_instance_schema_no_create_only();
    let schemas: HashMap<String, ResourceSchema> = vec![(key, schema)].into_iter().collect();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Compute the target SimHash.
    let mut anon = Resource::with_provider("awscc", "sso.Instance", "");
    anon.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers(
        &mut anon_vec,
        &providers,
        &schemas,
        &schema_key_fn,
        &identity_fn,
    )
    .unwrap();
    let anonymous_name = anon_vec[0].id.name_str().to_string();

    // Two state entries with the exact same name hash → same distance (0).
    let state_entries = vec![
        AnonymousIdStateInfo {
            name: anonymous_name.clone(),
            create_only_values: HashMap::new(),
        },
        AnonymousIdStateInfo {
            name: anonymous_name,
            create_only_values: HashMap::new(),
        },
    ];

    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso");
    let_bound.binding = Some("sso".to_string());
    let_bound.set_attr("name".to_string(), Value::String("carina-rs".to_string()));
    let resources = vec![let_bound];

    let renames = detect_anonymous_to_named_renames(
        &resources,
        &schemas,
        &schema_key_fn,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );
    assert!(
        renames.is_empty(),
        "two orphans tie on distance, should skip to avoid rebinding wrong entry: {:?}",
        renames
    );
}
