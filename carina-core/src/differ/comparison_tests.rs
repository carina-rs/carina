use super::*;

use indexmap::IndexMap;

#[test]
fn type_aware_int_float_coercion() {
    assert!(type_aware_equal(
        &Value::Int(42),
        &Value::Float(42.0),
        Some(&AttributeType::Float),
        None,
    ));
    assert!(type_aware_equal(
        &Value::Float(42.0),
        &Value::Int(42),
        Some(&AttributeType::Float),
        None,
    ));
    // Non-exact conversion should not be equal
    assert!(!type_aware_equal(
        &Value::Int(42),
        &Value::Float(42.5),
        Some(&AttributeType::Float),
        None,
    ));
    // Without type info, Int and Float are not equal
    assert!(!type_aware_equal(
        &Value::Int(42),
        &Value::Float(42.0),
        None,
        None,
    ));
}

#[test]
fn type_aware_int_float_coercion_for_int_type() {
    // Int type also allows coercion (e.g., provider returns Float for an Int field)
    assert!(type_aware_equal(
        &Value::Int(10),
        &Value::Float(10.0),
        Some(&AttributeType::Int),
        None,
    ));
}

#[test]
fn type_aware_list_with_inner_type() {
    let list_type = AttributeType::unordered_list(AttributeType::Float);
    // List of Int vs Float with coercion (unordered, so reordering is fine)
    assert!(type_aware_equal(
        &Value::List(vec![Value::Int(1), Value::Int(2)]),
        &Value::List(vec![Value::Float(2.0), Value::Float(1.0)]),
        Some(&list_type),
        None,
    ));
}

#[test]
fn type_aware_struct_per_field() {
    use crate::schema::StructField;

    let struct_type = AttributeType::Struct {
        name: "Config".to_string(),
        fields: vec![
            StructField::new("count", AttributeType::Float),
            StructField::new("name", AttributeType::String),
        ],
    };
    let a = Value::Map(IndexMap::from([
        ("count".to_string(), Value::Int(5)),
        ("name".to_string(), Value::String("test".to_string())),
    ]));
    let b = Value::Map(IndexMap::from([
        ("count".to_string(), Value::Float(5.0)),
        ("name".to_string(), Value::String("test".to_string())),
    ]));
    assert!(type_aware_equal(&a, &b, Some(&struct_type), None));
}

#[test]
fn type_aware_union_numeric() {
    let union_type = AttributeType::Union(vec![AttributeType::Int, AttributeType::Float]);
    assert!(type_aware_equal(
        &Value::Int(7),
        &Value::Float(7.0),
        Some(&union_type),
        None,
    ));
}

#[test]
fn type_aware_custom_delegates_to_base() {
    let custom_type = AttributeType::Custom {
        semantic_name: Some("Port".to_string()),
        base: Box::new(AttributeType::Float),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert!(type_aware_equal(
        &Value::Int(8080),
        &Value::Float(8080.0),
        Some(&custom_type),
        None,
    ));
}

#[test]
fn type_aware_diff_no_change_with_schema() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let mut schema = ResourceSchema::new("test.resource");
    schema.attributes.insert(
        "port".to_string(),
        AttributeSchema::new("port", AttributeType::Float),
    );

    let desired = Resource::new("test.resource", "test").with_attribute("port", Value::Int(443));

    let mut current_attrs = HashMap::new();
    current_attrs.insert("port".to_string(), Value::Float(443.0));
    let current = State::existing(ResourceId::new("test.resource", "test"), current_attrs);

    // Without schema: detects a change (Int != Float)
    let result = diff(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::Update { .. }),
        "Without schema, Int(443) != Float(443.0) should be Update, got {:?}",
        result
    );

    // With schema: no change (type-aware coercion)
    let result = diff(&desired, &current, None, None, Some(&schema));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "With schema, Int(443) and Float(443.0) should be NoChange, got {:?}",
        result
    );
}

#[test]
fn type_aware_struct_ignores_default_bool_false() {
    use crate::schema::StructField;

    // Struct with an optional bool field (bucket_key_enabled)
    let struct_type = AttributeType::Struct {
        name: "ServerSideEncryptionRule".to_string(),
        fields: vec![
            StructField::new("bucket_key_enabled", AttributeType::Bool),
            StructField::new("sse_algorithm", AttributeType::String),
        ],
    };

    // Desired: only sse_algorithm specified (no bucket_key_enabled)
    let desired = Value::Map(IndexMap::from([(
        "sse_algorithm".to_string(),
        Value::String("AES256".to_string()),
    )]));

    // Current (from AWS): includes bucket_key_enabled: false as default
    let current = Value::Map(IndexMap::from([
        ("bucket_key_enabled".to_string(), Value::Bool(false)),
        (
            "sse_algorithm".to_string(),
            Value::String("AES256".to_string()),
        ),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type), None),
        "Struct with extra default Bool(false) should be considered equal"
    );
}

#[test]
fn type_aware_struct_does_not_ignore_non_default_bool() {
    use crate::schema::StructField;

    let struct_type = AttributeType::Struct {
        name: "ServerSideEncryptionRule".to_string(),
        fields: vec![
            StructField::new("bucket_key_enabled", AttributeType::Bool),
            StructField::new("sse_algorithm", AttributeType::String),
        ],
    };

    // Desired: only sse_algorithm
    let desired = Value::Map(IndexMap::from([(
        "sse_algorithm".to_string(),
        Value::String("AES256".to_string()),
    )]));

    // Current: bucket_key_enabled is true (non-default) — should NOT be equal
    let current = Value::Map(IndexMap::from([
        ("bucket_key_enabled".to_string(), Value::Bool(true)),
        (
            "sse_algorithm".to_string(),
            Value::String("AES256".to_string()),
        ),
    ]));

    assert!(
        !type_aware_equal(&desired, &current, Some(&struct_type), None),
        "Struct with non-default Bool(true) should NOT be considered equal"
    );
}

#[test]
fn type_aware_string_enum_namespaced_vs_raw() {
    // StringEnum with namespace
    let enum_type = AttributeType::StringEnum {
        name: "ServerSideEncryptionByDefaultSseAlgorithm".to_string(),
        values: vec![
            "aws:kms".to_string(),
            "AES256".to_string(),
            "aws:kms:dsse".to_string(),
        ],
        namespace: Some("awscc.s3.Bucket".to_string()),
        to_dsl: None,
    };

    // Namespaced form vs raw string
    assert!(
        type_aware_equal(
            &Value::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            &Value::String("AES256".to_string()),
            Some(&enum_type),
            None,
        ),
        "Namespaced enum and raw value should be considered equal"
    );

    // Both in namespaced form
    assert!(
        type_aware_equal(
            &Value::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            &Value::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            Some(&enum_type),
            None,
        ),
        "Both namespaced should be equal"
    );

    // Different values should not match
    assert!(
        !type_aware_equal(
            &Value::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            &Value::String("aws:kms".to_string()),
            Some(&enum_type),
            None,
        ),
        "Different enum values should not be equal"
    );
}

#[test]
fn type_aware_struct_ignores_default_string_enum_empty() {
    use crate::schema::StructField;

    let struct_type = AttributeType::Struct {
        name: "Config".to_string(),
        fields: vec![
            StructField::new("name", AttributeType::String),
            StructField::new(
                "status",
                AttributeType::StringEnum {
                    name: "Status".to_string(),
                    values: vec!["Active".to_string(), "Inactive".to_string()],
                    namespace: None,
                    to_dsl: None,
                },
            ),
        ],
    };

    // Desired: only name specified
    let desired = Value::Map(IndexMap::from([(
        "name".to_string(),
        Value::String("test".to_string()),
    )]));

    // Current: includes status: "" as default
    let current = Value::Map(IndexMap::from([
        ("name".to_string(), Value::String("test".to_string())),
        ("status".to_string(), Value::String(String::new())),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type), None),
        "Struct with extra default StringEnum empty string should be considered equal"
    );
}

#[test]
fn type_aware_struct_ignores_default_custom_type() {
    use crate::schema::StructField;

    let struct_type = AttributeType::Struct {
        name: "Config".to_string(),
        fields: vec![
            StructField::new("name", AttributeType::String),
            StructField::new(
                "port",
                AttributeType::Custom {
                    semantic_name: Some("Port".to_string()),
                    base: Box::new(AttributeType::Int),
                    pattern: None,
                    length: None,
                    validate: |_| Ok(()),
                    namespace: None,
                    to_dsl: None,
                },
            ),
        ],
    };

    // Desired: only name specified
    let desired = Value::Map(IndexMap::from([(
        "name".to_string(),
        Value::String("test".to_string()),
    )]));

    // Current: includes port: 0 as default
    let current = Value::Map(IndexMap::from([
        ("name".to_string(), Value::String("test".to_string())),
        ("port".to_string(), Value::Int(0)),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type), None),
        "Struct with extra default Custom(Int) zero should be considered equal"
    );
}

#[test]
fn type_aware_struct_ignores_default_nested_struct_empty() {
    use crate::schema::StructField;

    let struct_type = AttributeType::Struct {
        name: "Outer".to_string(),
        fields: vec![
            StructField::new("name", AttributeType::String),
            StructField::new(
                "inner",
                AttributeType::Struct {
                    name: "Inner".to_string(),
                    fields: vec![StructField::new("value", AttributeType::String)],
                },
            ),
        ],
    };

    // Desired: only name specified
    let desired = Value::Map(IndexMap::from([(
        "name".to_string(),
        Value::String("test".to_string()),
    )]));

    // Current: includes inner: {} as default
    let current = Value::Map(IndexMap::from([
        ("name".to_string(), Value::String("test".to_string())),
        ("inner".to_string(), Value::Map(IndexMap::new())),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type), None),
        "Struct with extra default nested Struct empty map should be considered equal"
    );
}

#[test]
fn type_aware_ordered_list_detects_reorder() {
    // An ordered list (insertionOrder=true) should detect reordering as a change
    let ordered_list_type = AttributeType::List {
        inner: Box::new(AttributeType::String),
        ordered: true,
    };

    // Same elements, different order
    let a = Value::List(vec![
        Value::String("a".to_string()),
        Value::String("b".to_string()),
    ]);
    let b = Value::List(vec![
        Value::String("b".to_string()),
        Value::String("a".to_string()),
    ]);

    assert!(
        !type_aware_equal(&a, &b, Some(&ordered_list_type), None),
        "Ordered list should detect reorder as NOT equal"
    );

    // Same elements, same order should still be equal
    let c = Value::List(vec![
        Value::String("a".to_string()),
        Value::String("b".to_string()),
    ]);
    assert!(
        type_aware_equal(&a, &c, Some(&ordered_list_type), None),
        "Ordered list with same order should be equal"
    );
}

#[test]
fn type_aware_unordered_list_ignores_reorder() {
    // An unordered list (insertionOrder=false) should treat reordering as no change
    let unordered_list_type = AttributeType::List {
        inner: Box::new(AttributeType::String),
        ordered: false,
    };

    let a = Value::List(vec![
        Value::String("a".to_string()),
        Value::String("b".to_string()),
    ]);
    let b = Value::List(vec![
        Value::String("b".to_string()),
        Value::String("a".to_string()),
    ]);

    assert!(
        type_aware_equal(&a, &b, Some(&unordered_list_type), None),
        "Unordered list should treat reorder as equal"
    );
}

// --- Tests for write-only attribute skip in find_changed_attributes ---

#[test]
fn write_only_attr_in_desired_not_in_current_no_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only());

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        ),
        ("ipv4_netmask_length".to_string(), Value::Int(16)),
    ]);
    // CloudControl Read API does not return write-only attributes
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    )]);

    let changed = find_changed_attributes(&desired, &current, None, None, Some(&schema), None);
    assert!(
        changed.is_empty(),
        "Write-only attribute absent from current should not trigger a diff, got: {:?}",
        changed
    );
}

#[test]
fn write_only_attr_in_both_same_value_no_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only());

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        ),
        ("ipv4_netmask_length".to_string(), Value::Int(16)),
    ]);
    let current = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        ),
        ("ipv4_netmask_length".to_string(), Value::Int(16)),
    ]);

    let changed = find_changed_attributes(&desired, &current, None, None, Some(&schema), None);
    assert!(
        changed.is_empty(),
        "Write-only attribute with same value should not trigger a diff, got: {:?}",
        changed
    );
}

#[test]
fn write_only_attr_in_both_different_value_detects_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only());

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        ),
        ("ipv4_netmask_length".to_string(), Value::Int(24)),
    ]);
    let current = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        ),
        ("ipv4_netmask_length".to_string(), Value::Int(16)),
    ]);

    let changed = find_changed_attributes(&desired, &current, None, None, Some(&schema), None);
    assert!(
        changed.contains(&"ipv4_netmask_length".to_string()),
        "Write-only attribute with different value should trigger a diff"
    );
}

#[test]
fn non_write_only_attr_in_desired_not_in_current_detects_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new("enable_dns", AttributeType::Bool));

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        ),
        ("enable_dns".to_string(), Value::Bool(true)),
    ]);
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    )]);

    let changed = find_changed_attributes(&desired, &current, None, None, Some(&schema), None);
    assert!(
        changed.contains(&"enable_dns".to_string()),
        "Non-write-only attribute absent from current should trigger a diff"
    );
}

#[test]
fn secret_unchanged_same_hash() {
    use crate::value::value_to_json;

    let secret_value = Value::Secret(Box::new(Value::String("my-password".to_string())));
    // Compute the hash that would be stored in state
    let hash_json = value_to_json(&secret_value).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    // State has the hash string, desired has the Secret wrapper
    assert!(type_aware_equal(
        &secret_value,
        &Value::String(hash_str.clone()),
        None,
        None,
    ));
    // Reversed order should also work
    assert!(type_aware_equal(
        &Value::String(hash_str),
        &secret_value,
        None,
        None,
    ));
}

#[test]
fn secret_changed_different_hash() {
    use crate::value::value_to_json;

    let old_secret = Value::Secret(Box::new(Value::String("old-password".to_string())));
    let new_secret = Value::Secret(Box::new(Value::String("new-password".to_string())));
    // Compute the hash for the OLD secret (stored in state)
    let old_hash_json = value_to_json(&old_secret).unwrap();
    let old_hash_str = old_hash_json.as_str().unwrap().to_string();

    // New desired vs old state hash should be different
    assert!(!type_aware_equal(
        &new_secret,
        &Value::String(old_hash_str),
        None,
        None,
    ));
}

#[test]
fn secret_in_find_changed_attributes_no_change() {
    use crate::value::value_to_json;

    let secret_value = Value::Secret(Box::new(Value::String("my-password".to_string())));
    let hash_json = value_to_json(&secret_value).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("password".to_string(), secret_value)]);
    let current = HashMap::from([("password".to_string(), Value::String(hash_str))]);

    let changed = find_changed_attributes(&desired, &current, None, None, None, None);
    assert!(
        changed.is_empty(),
        "Secret with matching hash should not show as changed, got: {:?}",
        changed
    );
}

#[test]
fn secret_in_find_changed_attributes_changed() {
    use crate::value::value_to_json;

    let old_secret = Value::Secret(Box::new(Value::String("old-password".to_string())));
    let new_secret = Value::Secret(Box::new(Value::String("new-password".to_string())));
    let old_hash_json = value_to_json(&old_secret).unwrap();
    let old_hash_str = old_hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("password".to_string(), new_secret)]);
    let current = HashMap::from([("password".to_string(), Value::String(old_hash_str))]);

    let changed = find_changed_attributes(&desired, &current, None, None, None, None);
    assert!(
        changed.contains(&"password".to_string()),
        "Secret with different hash should show as changed"
    );
}

#[test]
fn secret_in_map_no_change_when_hash_matches() {
    use crate::value::value_to_json;

    // Desired: tags map with a secret value
    let secret_value = Value::Secret(Box::new(Value::String("super-secret".to_string())));
    let desired_tags = Value::Map(IndexMap::from([
        ("Name".to_string(), Value::String("test".to_string())),
        ("SecretTag".to_string(), secret_value.clone()),
    ]));

    // State: tags map with the secret hash (as stored by from_provider_state)
    let hash_json = value_to_json(&secret_value).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();
    let state_tags = Value::Map(IndexMap::from([
        ("Name".to_string(), Value::String("test".to_string())),
        ("SecretTag".to_string(), Value::String(hash_str)),
    ]));

    let desired = HashMap::from([("tags".to_string(), desired_tags)]);
    let current = HashMap::from([("tags".to_string(), state_tags)]);

    let changed = find_changed_attributes(&desired, &current, None, None, None, None);
    assert!(
        changed.is_empty(),
        "Secret in map with matching hash should not show as changed, got: {:?}",
        changed
    );
}

#[test]
fn secret_with_context_no_change_when_hash_matches() {
    use crate::resource::ResourceId;
    use crate::value::{SecretHashContext, value_to_json_with_context};

    let resource_id = ResourceId::with_provider("awscc", "rds.db_instance", "my-db");
    let ctx = SecretHashContext::new(
        resource_id.display_type(),
        resource_id.name_str(),
        "master_password",
    );

    let secret_value = Value::Secret(Box::new(Value::String("my-password".to_string())));
    // Hash with context (as from_provider_state would do)
    let hash_json = value_to_json_with_context(&secret_value, Some(&ctx)).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("master_password".to_string(), secret_value)]);
    let current = HashMap::from([("master_password".to_string(), Value::String(hash_str))]);

    // find_changed_attributes builds context from resource_id
    let changed = find_changed_attributes(&desired, &current, None, None, None, Some(&resource_id));
    assert!(
        changed.is_empty(),
        "Secret with matching context-hashed value should not show as changed, got: {:?}",
        changed
    );
}

#[test]
fn secret_with_context_detects_change() {
    use crate::resource::ResourceId;
    use crate::value::{SecretHashContext, value_to_json_with_context};

    let resource_id = ResourceId::with_provider("awscc", "rds.db_instance", "my-db");
    let ctx = SecretHashContext::new(
        resource_id.display_type(),
        resource_id.name_str(),
        "master_password",
    );

    let old_secret = Value::Secret(Box::new(Value::String("old-password".to_string())));
    let new_secret = Value::Secret(Box::new(Value::String("new-password".to_string())));
    // Hash the OLD secret with context
    let hash_json = value_to_json_with_context(&old_secret, Some(&ctx)).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("master_password".to_string(), new_secret)]);
    let current = HashMap::from([("master_password".to_string(), Value::String(hash_str))]);

    let changed = find_changed_attributes(&desired, &current, None, None, None, Some(&resource_id));
    assert!(
        changed.contains(&"master_password".to_string()),
        "Secret with different value should show as changed with context"
    );
}

#[test]
fn secret_same_password_different_resources_produces_different_hashes() {
    use crate::resource::ResourceId;
    use crate::value::{SecretHashContext, value_to_json_with_context};

    let secret = Value::Secret(Box::new(Value::String("shared-password".to_string())));

    let ctx1 = SecretHashContext::new("awscc.rds.db_instance", "db-1", "master_password");
    let ctx2 = SecretHashContext::new("awscc.rds.db_instance", "db-2", "master_password");

    let hash1 = value_to_json_with_context(&secret, Some(&ctx1)).unwrap();
    let hash2 = value_to_json_with_context(&secret, Some(&ctx2)).unwrap();

    assert_ne!(
        hash1, hash2,
        "Same password on different resources should produce different state hashes"
    );

    // Each hash should match its own context
    let id1 = ResourceId::with_provider("awscc", "rds.db_instance", "db-1");
    let desired1 = HashMap::from([("master_password".to_string(), secret.clone())]);
    let current1 = HashMap::from([(
        "master_password".to_string(),
        Value::String(hash1.as_str().unwrap().to_string()),
    )]);
    let changed1 = find_changed_attributes(&desired1, &current1, None, None, None, Some(&id1));
    assert!(
        changed1.is_empty(),
        "Hash should match its own resource context"
    );

    // But not the other resource's context
    let id2 = ResourceId::with_provider("awscc", "rds.db_instance", "db-2");
    let desired2 = HashMap::from([("master_password".to_string(), secret)]);
    let current2 = HashMap::from([(
        "master_password".to_string(),
        Value::String(hash1.as_str().unwrap().to_string()),
    )]);
    let changed2 = find_changed_attributes(&desired2, &current2, None, None, None, Some(&id2));
    assert!(
        changed2.contains(&"master_password".to_string()),
        "Hash from db-1 should not match db-2's context"
    );
}

/// Regression test for #1249: Secret compared against plain-text state value
/// (as returned by provider.read in refresh=true mode) should match when
/// the inner values are equal.
#[test]
fn secret_matches_plain_text_state_value() {
    let secret = Value::Secret(Box::new(Value::String("my-password".to_string())));
    let plain = Value::String("my-password".to_string());
    assert!(
        type_aware_equal(&secret, &plain, None, None),
        "Secret should match plain-text state when inner values are equal"
    );
    assert!(
        type_aware_equal(&plain, &secret, None, None),
        "Plain-text state should match secret when inner values are equal"
    );
}

/// Secret with different inner value should NOT match plain-text state.
#[test]
fn secret_does_not_match_different_plain_text() {
    let secret = Value::Secret(Box::new(Value::String("my-password".to_string())));
    let plain = Value::String("other-password".to_string());
    assert!(
        !type_aware_equal(&secret, &plain, None, None),
        "Secret should not match different plain-text state"
    );
}

/// Regression test for #1249: secret nested inside a Map (e.g., tags) with
/// refresh=true. After apply, plan reads from the provider which returns
/// the plain-text tag value. The differ must recognize that
/// Secret("super-secret-value") matches String("super-secret-value") when
/// the state value is the raw provider response (not a hash).
#[test]
fn secret_in_map_with_refresh_no_false_diff() {
    use crate::resource::ResourceId;
    use crate::schema::{AttributeSchema, ResourceSchema};

    let resource_id = ResourceId::with_provider("awscc", "ec2.Vpc", "ec2_vpc_fb75c929");

    // Desired: tags map with a secret value (as written in .crn)
    let desired_tags = Value::Map(IndexMap::from([
        ("Name".to_string(), Value::String("test".to_string())),
        (
            "SecretTag".to_string(),
            Value::Secret(Box::new(Value::String("super-secret-value".to_string()))),
        ),
    ]));

    // Current state from provider read (refresh=true): plain-text values
    let current_tags = Value::Map(IndexMap::from([
        ("Name".to_string(), Value::String("test".to_string())),
        (
            "SecretTag".to_string(),
            Value::String("super-secret-value".to_string()),
        ),
    ]));

    // Build schema with tags as Map(String)
    let schema = ResourceSchema::new("ec2.Vpc").attribute(AttributeSchema::new(
        "tags",
        AttributeType::map(AttributeType::String),
    ));

    let desired = HashMap::from([("tags".to_string(), desired_tags)]);
    let current = HashMap::from([("tags".to_string(), current_tags)]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        None,
        Some(&schema),
        Some(&resource_id),
    );
    assert!(
        changed.is_empty(),
        "Secret in map should not show false diff when current has plain-text from provider (issue #1249), got changed: {:?}",
        changed,
    );
}
