use super::*;

use crate::resource::{ConcreteValue, DeferredValue, Value};
use crate::schema::noop_validator;
use indexmap::IndexMap;

#[test]
fn type_aware_int_float_coercion() {
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(42)),
        &Value::Concrete(ConcreteValue::Float(42.0)),
        Some(&AttributeType::Float),
        None,
    ));
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Float(42.0)),
        &Value::Concrete(ConcreteValue::Int(42)),
        Some(&AttributeType::Float),
        None,
    ));
    // Non-exact conversion should not be equal
    assert!(!type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(42)),
        &Value::Concrete(ConcreteValue::Float(42.5)),
        Some(&AttributeType::Float),
        None,
    ));
    // Without type info, Int and Float are not equal
    assert!(!type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(42)),
        &Value::Concrete(ConcreteValue::Float(42.0)),
        None,
        None,
    ));
}

#[test]
fn type_aware_int_float_coercion_for_int_type() {
    // Int type also allows coercion (e.g., provider returns Float for an Int field)
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(10)),
        &Value::Concrete(ConcreteValue::Float(10.0)),
        Some(&AttributeType::Int),
        None,
    ));
}

#[test]
fn type_aware_list_with_inner_type() {
    let list_type = AttributeType::unordered_list(AttributeType::Float);
    // List of Int vs Float with coercion (unordered, so reordering is fine)
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Int(2))
        ])),
        &Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Float(2.0)),
            Value::Concrete(ConcreteValue::Float(1.0))
        ])),
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
    let a = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("count".to_string(), Value::Concrete(ConcreteValue::Int(5))),
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
    ])));
    let b = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "count".to_string(),
            Value::Concrete(ConcreteValue::Float(5.0)),
        ),
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
    ])));
    assert!(type_aware_equal(&a, &b, Some(&struct_type), None));
}

#[test]
fn type_aware_union_numeric() {
    let union_type = AttributeType::Union(vec![AttributeType::Int, AttributeType::Float]);
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(7)),
        &Value::Concrete(ConcreteValue::Float(7.0)),
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
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
    };
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(8080)),
        &Value::Concrete(ConcreteValue::Float(8080.0)),
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

    let desired = Resource::new("test.resource", "test")
        .with_attribute("port", Value::Concrete(ConcreteValue::Int(443)));

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "port".to_string(),
        Value::Concrete(ConcreteValue::Float(443.0)),
    );
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
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "sse_algorithm".to_string(),
        Value::Concrete(ConcreteValue::String("AES256".to_string())),
    )])));

    // Current (from AWS): includes bucket_key_enabled: false as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "bucket_key_enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        ),
        (
            "sse_algorithm".to_string(),
            Value::Concrete(ConcreteValue::String("AES256".to_string())),
        ),
    ])));

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
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "sse_algorithm".to_string(),
        Value::Concrete(ConcreteValue::String("AES256".to_string())),
    )])));

    // Current: bucket_key_enabled is true (non-default) — should NOT be equal
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "bucket_key_enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "sse_algorithm".to_string(),
            Value::Concrete(ConcreteValue::String("AES256".to_string())),
        ),
    ])));

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
        dsl_aliases: vec![],
    };

    // Namespaced form vs raw string
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            &Value::Concrete(ConcreteValue::String("AES256".to_string())),
            Some(&enum_type),
            None,
        ),
        "Namespaced enum and raw value should be considered equal"
    );

    // Both in namespaced form
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            &Value::Concrete(ConcreteValue::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            Some(&enum_type),
            None,
        ),
        "Both namespaced should be equal"
    );

    // Different values should not match
    assert!(
        !type_aware_equal(
            &Value::Concrete(ConcreteValue::String(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            &Value::Concrete(ConcreteValue::String("aws:kms".to_string())),
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
                    dsl_aliases: vec![],
                },
            ),
        ],
    };

    // Desired: only name specified
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    )])));

    // Current: includes status: "" as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        (
            "status".to_string(),
            Value::Concrete(ConcreteValue::String(String::new())),
        ),
    ])));

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
                    validate: noop_validator(),
                    namespace: None,
                    to_dsl: None,
                },
            ),
        ],
    };

    // Desired: only name specified
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    )])));

    // Current: includes port: 0 as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        ("port".to_string(), Value::Concrete(ConcreteValue::Int(0))),
    ])));

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
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    )])));

    // Current: includes inner: {} as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        (
            "inner".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        ),
    ])));

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
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".to_string())),
        Value::Concrete(ConcreteValue::String("b".to_string())),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("b".to_string())),
        Value::Concrete(ConcreteValue::String("a".to_string())),
    ]));

    assert!(
        !type_aware_equal(&a, &b, Some(&ordered_list_type), None),
        "Ordered list should detect reorder as NOT equal"
    );

    // Same elements, same order should still be equal
    let c = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".to_string())),
        Value::Concrete(ConcreteValue::String("b".to_string())),
    ]));
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

    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".to_string())),
        Value::Concrete(ConcreteValue::String("b".to_string())),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("b".to_string())),
        Value::Concrete(ConcreteValue::String("a".to_string())),
    ]));

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
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);
    // CloudControl Read API does not return write-only attributes
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
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
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);
    let current = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
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
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(24)),
        ),
    ]);
    let current = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
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
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        ),
        (
            "enable_dns".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ]);
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
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

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    // Compute the hash that would be stored in state
    let hash_json = value_to_json(&secret_value).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    // State has the hash string, desired has the Secret wrapper
    assert!(type_aware_equal(
        &secret_value,
        &Value::Concrete(ConcreteValue::String(hash_str.clone())),
        None,
        None,
    ));
    // Reversed order should also work
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::String(hash_str)),
        &secret_value,
        None,
        None,
    ));
}

#[test]
fn secret_changed_different_hash() {
    use crate::value::value_to_json;

    let old_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("old-password".to_string()),
    ))));
    let new_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("new-password".to_string()),
    ))));
    // Compute the hash for the OLD secret (stored in state)
    let old_hash_json = value_to_json(&old_secret).unwrap();
    let old_hash_str = old_hash_json.as_str().unwrap().to_string();

    // New desired vs old state hash should be different
    assert!(!type_aware_equal(
        &new_secret,
        &Value::Concrete(ConcreteValue::String(old_hash_str)),
        None,
        None,
    ));
}

#[test]
fn secret_in_find_changed_attributes_no_change() {
    use crate::value::value_to_json;

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    let hash_json = value_to_json(&secret_value).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("password".to_string(), secret_value)]);
    let current = HashMap::from([(
        "password".to_string(),
        Value::Concrete(ConcreteValue::String(hash_str)),
    )]);

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

    let old_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("old-password".to_string()),
    ))));
    let new_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("new-password".to_string()),
    ))));
    let old_hash_json = value_to_json(&old_secret).unwrap();
    let old_hash_str = old_hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("password".to_string(), new_secret)]);
    let current = HashMap::from([(
        "password".to_string(),
        Value::Concrete(ConcreteValue::String(old_hash_str)),
    )]);

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
    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("super-secret".to_string()),
    ))));
    let desired_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        ("SecretTag".to_string(), secret_value.clone()),
    ])));

    // State: tags map with the secret hash (as stored by from_provider_state)
    let hash_json = value_to_json(&secret_value).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();
    let state_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        (
            "SecretTag".to_string(),
            Value::Concrete(ConcreteValue::String(hash_str)),
        ),
    ])));

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
    use crate::resource::{ConcreteValue, DeferredValue, ResourceId};
    use crate::value::{SecretHashContext, value_to_json_with_context};

    let resource_id = ResourceId::with_provider("awscc", "rds.db_instance", "my-db", None);
    let ctx = SecretHashContext::new(
        resource_id.display_type(),
        resource_id.name_str(),
        "master_password",
    );

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    // Hash with context (as from_provider_state would do)
    let hash_json = value_to_json_with_context(&secret_value, Some(&ctx)).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("master_password".to_string(), secret_value)]);
    let current = HashMap::from([(
        "master_password".to_string(),
        Value::Concrete(ConcreteValue::String(hash_str)),
    )]);

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

    let resource_id = ResourceId::with_provider("awscc", "rds.db_instance", "my-db", None);
    let ctx = SecretHashContext::new(
        resource_id.display_type(),
        resource_id.name_str(),
        "master_password",
    );

    let old_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("old-password".to_string()),
    ))));
    let new_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("new-password".to_string()),
    ))));
    // Hash the OLD secret with context
    let hash_json = value_to_json_with_context(&old_secret, Some(&ctx)).unwrap();
    let hash_str = hash_json.as_str().unwrap().to_string();

    let desired = HashMap::from([("master_password".to_string(), new_secret)]);
    let current = HashMap::from([(
        "master_password".to_string(),
        Value::Concrete(ConcreteValue::String(hash_str)),
    )]);

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

    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("shared-password".to_string()),
    ))));

    let ctx1 = SecretHashContext::new("awscc.rds.db_instance", "db-1", "master_password");
    let ctx2 = SecretHashContext::new("awscc.rds.db_instance", "db-2", "master_password");

    let hash1 = value_to_json_with_context(&secret, Some(&ctx1)).unwrap();
    let hash2 = value_to_json_with_context(&secret, Some(&ctx2)).unwrap();

    assert_ne!(
        hash1, hash2,
        "Same password on different resources should produce different state hashes"
    );

    // Each hash should match its own context
    let id1 = ResourceId::with_provider("awscc", "rds.db_instance", "db-1", None);
    let desired1 = HashMap::from([("master_password".to_string(), secret.clone())]);
    let current1 = HashMap::from([(
        "master_password".to_string(),
        Value::Concrete(ConcreteValue::String(hash1.as_str().unwrap().to_string())),
    )]);
    let changed1 = find_changed_attributes(&desired1, &current1, None, None, None, Some(&id1));
    assert!(
        changed1.is_empty(),
        "Hash should match its own resource context"
    );

    // But not the other resource's context
    let id2 = ResourceId::with_provider("awscc", "rds.db_instance", "db-2", None);
    let desired2 = HashMap::from([("master_password".to_string(), secret)]);
    let current2 = HashMap::from([(
        "master_password".to_string(),
        Value::Concrete(ConcreteValue::String(hash1.as_str().unwrap().to_string())),
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
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    let plain = Value::Concrete(ConcreteValue::String("my-password".to_string()));
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
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    let plain = Value::Concrete(ConcreteValue::String("other-password".to_string()));
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

    let resource_id = ResourceId::with_provider("awscc", "ec2.Vpc", "ec2_vpc_fb75c929", None);

    // Desired: tags map with a secret value (as written in .crn)
    let desired_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        (
            "SecretTag".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("super-secret-value".to_string()),
            )))),
        ),
    ])));

    // Current state from provider read (refresh=true): plain-text values
    let current_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        ),
        (
            "SecretTag".to_string(),
            Value::Concrete(ConcreteValue::String("super-secret-value".to_string())),
        ),
    ])));

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

// ---- Union[String, list(String)] canonical-form invariant (#2481, #2512) ----

/// Helper: build the `Union[String, list(String)]` shape used by
/// IAM-style `string_or_list_of_strings` schema fields.
fn string_or_list_of_strings_type() -> AttributeType {
    AttributeType::Union(vec![
        AttributeType::String,
        AttributeType::list(AttributeType::String),
    ])
}

#[test]
fn union_string_or_list_canonical_string_list_equal_to_self() {
    // Two `Value::Concrete(ConcreteValue::StringList)` values with identical contents must be
    // equal under the differ for the `Union[String, list(String)]`
    // type. Sanity check that the canonical form is comparable at all.
    let union = string_or_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".to_string()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".to_string()]));
    assert!(type_aware_equal(&a, &b, Some(&union), None));
}

#[test]
fn union_string_or_list_canonical_string_list_diff_on_different_content() {
    let union = string_or_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".to_string()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["repo:bar:*".to_string()]));
    assert!(!type_aware_equal(&a, &b, Some(&union), None));
}

#[test]
fn union_string_or_list_non_canonical_mixed_shapes_fail_to_equal() {
    // Invariant guard (#2512): a non-canonical pair must NOT be
    // treated as equal by the differ. If the canonicalization upstream
    // (#2511 / #2513) ever regresses, this test catches the resulting
    // phantom diff at its source instead of letting the comparator
    // paper over it. Adding a special-case equality here would defeat
    // the type-level canonicalization design.
    let union = string_or_list_of_strings_type();
    let scalar = Value::Concrete(ConcreteValue::String("repo:foo:*".to_string()));
    let canonical = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".to_string()]));
    assert!(!type_aware_equal(&scalar, &canonical, Some(&union), None));

    let legacy_list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::String("repo:foo:*".to_string()),
    )]));
    assert!(!type_aware_equal(
        &legacy_list,
        &canonical,
        Some(&union),
        None
    ));
}

#[test]
fn union_string_or_list_through_custom_wrapper() {
    // `Custom` wrappers around the union must remain transparent —
    // the comparator delegates `Custom { base, .. }` to its `base`.
    let inner = string_or_list_of_strings_type();
    let custom = AttributeType::Custom {
        semantic_name: Some("PolicyConditionValue".to_string()),
        base: Box::new(inner),
        pattern: None,
        length: None,
        validate: std::sync::Arc::new(|_| Ok(())),
        namespace: None,
        to_dsl: None,
    };
    let a = Value::Concrete(ConcreteValue::StringList(vec!["x".to_string()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["x".to_string()]));
    assert!(type_aware_equal(&a, &b, Some(&custom), None));
}

/// carina#3080 differ parity (design Test plan item 2+3): the
/// `principal` `Union[Struct{ service: Union[String, List<String>] },
/// String]` phantom must vanish at the **differ verdict**, and it must
/// vanish *because the pipeline canonicalizes both sides to the same
/// `StringList`* — NOT because of any comparator special-case (which
/// `comparison.rs:28-47` prohibits). This runs the real order:
/// `canonicalize_*_with_schemas` (the apply/plan path) → `diff`.
#[test]
fn carina3080_principal_scalar_vs_singleton_is_no_change_via_pipeline() {
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};
    use crate::value::{canonicalize_resources_with_schemas, canonicalize_states_with_schemas};

    let principal = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "PrincipalStruct".to_string(),
            fields: vec![StructField::new(
                "service",
                AttributeType::Union(vec![
                    AttributeType::String,
                    AttributeType::list(AttributeType::String),
                ]),
            )],
        },
        AttributeType::String,
    ]);
    let mut schema = ResourceSchema::new("iam.policy");
    schema.attributes.insert(
        "principal".to_string(),
        AttributeSchema::new("principal", principal),
    );
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert("aws", schema.clone());

    // Desired: user's bare scalar inside the Struct member.
    let mut desired_inner = IndexMap::new();
    desired_inner.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::String(
            "cloudfront.amazonaws.com".to_string(),
        )),
    );
    let mut resources = vec![Resource::new("iam.policy", "p1").with_attribute(
        "principal",
        Value::Concrete(ConcreteValue::Map(desired_inner)),
    )];
    // Force the provider segment so the registry lookup ("aws") hits.
    resources[0].id.provider = "aws".to_string();
    canonicalize_resources_with_schemas(&mut resources, &registry);

    // State: aws-read singleton list inside the Struct member.
    let mut state_inner = IndexMap::new();
    state_inner.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("cloudfront.amazonaws.com".to_string()),
        )])),
    );
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "principal".to_string(),
        Value::Concrete(ConcreteValue::Map(state_inner)),
    );
    let mut id = ResourceId::new("iam.policy", "p1");
    id.provider = "aws".to_string();
    let mut states = std::collections::HashMap::new();
    let st = State::existing(id, state_attrs);
    states.insert(st.id.clone(), st);
    canonicalize_states_with_schemas(&mut states, &registry);

    let current = states.into_values().next().unwrap();
    let result = diff(&resources[0], &current, None, None, Some(&schema));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3080: scalar (desired) vs singleton-list (state) under \
         Union[Struct,String] must be NoChange after the real \
         canonicalize pipeline — got {result:?}"
    );
}

/// carina#3122 SHAPE A: a steady-state no-op plan on awscc CloudFront
/// `Distribution` must NOT report a change for `allowed_methods` /
/// `cached_methods`. Those are nested two Struct levels deep
/// (`distribution_config` → `default_cache_behavior` →
/// `allowed_methods`) and schema-typed `unordered_list(StringEnum)`.
///
/// Representative subset modeling the same axes as the live plan
/// against `carina-rs/infra registry/dev/registry` (the verbatim live
/// capture is `tests/carina3122_live_phantom.rs`). Both sides reach
/// the differ as `ConcreteValue::String` in fully-qualified namespaced
/// DSL form; the values are identical and only the element ORDER
/// differs:
/// - State (`from`):   `[String("awscc.cloudfront.Distribution.AllowedMethods.head"),
///                        String("…AllowedMethods.get")]` — order head, get
/// - Desired (`to`):   `[String("…AllowedMethods.get"),
///                        String("…AllowedMethods.head")]` — order get, head
///
/// `allowed_methods` is schema-typed `unordered_list(StringEnum)`, so
/// element order is NOT significant: a multiset-equal pair must be
/// `NoChange`. A pure order-only difference, not a type or alias
/// mismatch.
///
/// Root cause of the live phantom was external to carina-core: the
/// infra stack pinned an old awscc revision whose generated schema
/// still used `AttributeType::list` (`ordered: true`) for these
/// fields; awscc HEAD already emits `unordered_list` (`ordered:
/// false`). carina-core itself is correct *given* `ordered: false`.
/// This test pins that contract: with the schema declaring the list
/// unordered, a value-equal/order-reversed pair is `NoChange`. It is
/// the regression guard so a future stale-pin recurrence is not
/// misattributed to carina-core.
///
/// Verdict reached through the real `canonicalize_*_with_schemas` →
/// `diff` pipeline (see `feedback_unit_test_path_is_not_apply_path`).
#[test]
fn carina3122_cloudfront_allowed_methods_set_is_no_change_via_pipeline() {
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};
    use crate::value::{canonicalize_resources_with_schemas, canonicalize_states_with_schemas};

    let method_enum = |name: &str, values: &[&str]| AttributeType::StringEnum {
        name: name.to_string(),
        values: values.iter().map(|s| s.to_string()).collect(),
        namespace: Some("awscc.cloudfront.Distribution".to_string()),
        dsl_aliases: values
            .iter()
            .map(|s| (s.to_string(), s.to_lowercase()))
            .collect(),
    };
    let default_cache_behavior = AttributeType::Struct {
        name: "DefaultCacheBehavior".to_string(),
        fields: vec![
            StructField::new(
                "allowed_methods",
                AttributeType::unordered_list(method_enum(
                    "AllowedMethods",
                    &["GET", "HEAD", "OPTIONS", "PUT", "PATCH", "POST", "DELETE"],
                )),
            )
            .with_provider_name("AllowedMethods"),
            StructField::new(
                "cached_methods",
                AttributeType::unordered_list(method_enum(
                    "CachedMethods",
                    &["GET", "HEAD", "OPTIONS"],
                )),
            )
            .with_provider_name("CachedMethods"),
            // User-authored scalar fields, so the test also covers the
            // "authored field survives projection and compares equal"
            // path alongside the read-back-default stripping.
            StructField::new("compress", AttributeType::Bool).with_provider_name("Compress"),
            StructField::new("target_origin_id", AttributeType::String)
                .with_provider_name("TargetOriginId"),
            StructField::new(
                "viewer_protocol_policy",
                method_enum(
                    "ViewerProtocolPolicy",
                    &["allow-all", "https-only", "redirect-to-https"],
                ),
            )
            .with_provider_name("ViewerProtocolPolicy"),
        ],
    };
    let distribution_config = AttributeType::Struct {
        name: "DistributionConfig".to_string(),
        fields: vec![
            StructField::new("default_cache_behavior", default_cache_behavior)
                .with_provider_name("DefaultCacheBehavior"),
        ],
    };
    let mut schema = ResourceSchema::new("cloudfront.Distribution");
    schema.attributes.insert(
        "distribution_config".to_string(),
        AttributeSchema::new("distribution_config", distribution_config),
    );
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert("awscc", schema.clone());

    // Both sides are the SAME namespaced DSL String shape (per the
    // live plan JSON); only element order differs.
    let ns_method = |type_name: &str, alias: &str| {
        Value::Concrete(ConcreteValue::String(format!(
            "awscc.cloudfront.Distribution.{type_name}.{alias}"
        )))
    };
    let methods = |type_name: &str, a: &str, b: &str| {
        Value::Concrete(ConcreteValue::List(vec![
            ns_method(type_name, a),
            ns_method(type_name, b),
        ]))
    };

    // Desired (`to`): order get, head. Includes the user-authored
    // scalar fields so they survive `prev_explicit` projection and are
    // compared (equal) on both sides — distinct from the read-back
    // defaults below which are projected away.
    let s = |v: &str| Value::Concrete(ConcreteValue::String(v.to_string()));
    let mut desired_dcb = IndexMap::new();
    desired_dcb.insert(
        "allowed_methods".to_string(),
        methods("AllowedMethods", "get", "head"),
    );
    desired_dcb.insert(
        "cached_methods".to_string(),
        methods("CachedMethods", "get", "head"),
    );
    desired_dcb.insert(
        "compress".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    desired_dcb.insert("target_origin_id".to_string(), s("s3-origin"));
    desired_dcb.insert(
        "viewer_protocol_policy".to_string(),
        s("awscc.cloudfront.Distribution.ViewerProtocolPolicy.redirect_to_https"),
    );
    let mut desired_dc = IndexMap::new();
    desired_dc.insert(
        "default_cache_behavior".to_string(),
        Value::Concrete(ConcreteValue::Map(desired_dcb)),
    );
    let mut resources = vec![
        Resource::new("cloudfront.Distribution", "d1").with_attribute(
            "distribution_config",
            Value::Concrete(ConcreteValue::Map(desired_dc)),
        ),
    ];
    resources[0].id.provider = "awscc".to_string();
    canonicalize_resources_with_schemas(&mut resources, &registry);

    // State (`from`): same namespaced String values, opposite order
    // (head, get) — the awscc `normalize_state` pass rewrites the
    // Cloud Control read-back to this fully-qualified DSL form. PLUS
    // the server-side read-back defaults the user never authored.
    // These exact keys/values are from the live plan `--json` diff of
    // `from` vs `to` (registry/dev/registry, state_serial 33). They
    // are present ONLY in `from` (state); `to` (desired) has just the
    // user-authored fields. Critically, several are NOT type-zero
    // (`http_version` is a non-empty StringEnum, `ipv6_enabled`/
    // `staging` are `true`), so `is_type_default` does NOT tolerate
    // them — they must instead be stripped by the `prev_explicit`
    // projection. This test exercises the real `diff` signature with
    // `prev_explicit` built from the desired resource (as the
    // carina-cli plan path does).
    let empty_list = || Value::Concrete(ConcreteValue::List(vec![]));
    let mut state_dcb = IndexMap::new();
    state_dcb.insert(
        "allowed_methods".to_string(),
        methods("AllowedMethods", "head", "get"),
    );
    state_dcb.insert(
        "cached_methods".to_string(),
        methods("CachedMethods", "head", "get"),
    );
    // User-authored default_cache_behavior fields — identical on both
    // sides, so they survive projection and compare equal.
    state_dcb.insert(
        "compress".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    state_dcb.insert("target_origin_id".to_string(), s("s3-origin"));
    state_dcb.insert(
        "viewer_protocol_policy".to_string(),
        s("awscc.cloudfront.Distribution.ViewerProtocolPolicy.redirect_to_https"),
    );
    // Read-back-only defaults inside default_cache_behavior (NOT in `to`).
    state_dcb.insert("field_level_encryption_id".to_string(), s(""));
    state_dcb.insert("function_associations".to_string(), empty_list());
    state_dcb.insert("lambda_function_associations".to_string(), empty_list());
    state_dcb.insert(
        "smooth_streaming".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    state_dcb.insert("trusted_key_groups".to_string(), empty_list());
    state_dcb.insert("trusted_signers".to_string(), empty_list());
    let mut grpc = IndexMap::new();
    grpc.insert(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    state_dcb.insert(
        "grpc_config".to_string(),
        Value::Concrete(ConcreteValue::Map(grpc)),
    );

    let mut state_dc = IndexMap::new();
    state_dc.insert(
        "default_cache_behavior".to_string(),
        Value::Concrete(ConcreteValue::Map(state_dcb)),
    );
    // Read-back-only defaults at distribution_config top level (NOT in
    // `to`). `http_version`/`ipv6_enabled`/`staging` are non-zero, so
    // only the prev_explicit projection can suppress them.
    state_dc.insert("cache_behaviors".to_string(), empty_list());
    state_dc.insert("continuous_deployment_policy_id".to_string(), s(""));
    state_dc.insert("custom_error_responses".to_string(), empty_list());
    state_dc.insert("default_root_object".to_string(), s(""));
    state_dc.insert(
        "http_version".to_string(),
        s("awscc.cloudfront.Distribution.HttpVersion.http1_1"),
    );
    state_dc.insert(
        "ipv6_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    state_dc.insert(
        "staging".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    state_dc.insert("web_acl_id".to_string(), s(""));
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "distribution_config".to_string(),
        Value::Concrete(ConcreteValue::Map(state_dc)),
    );
    let mut id = ResourceId::new("cloudfront.Distribution", "d1");
    id.provider = "awscc".to_string();
    let mut states = std::collections::HashMap::new();
    let st = State::existing(id, state_attrs);
    states.insert(st.id.clone(), st);
    canonicalize_states_with_schemas(&mut states, &registry);

    // The carina-cli plan path passes `prev_explicit` built from the
    // desired resource so server-side defaults are projected out of
    // the state side before comparison. Model that here.
    let prev_explicit = crate::explicit::build_from_resource(&resources[0]);

    let current = states.into_values().next().unwrap();
    let result = diff(
        &resources[0],
        &current,
        None,
        Some(&prev_explicit),
        Some(&schema),
    );
    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3122 SHAPE A: a steady-state no-op plan must be \
         NoChange. State carries server read-back defaults the user \
         never authored (http_version, ipv6_enabled, …) plus \
         order-reversed unordered List<StringEnum> allowed_methods/\
         cached_methods; prev_explicit projection + unordered-list \
         multiset must collapse all of it — got {result:?}"
    );
}

/// Contrast to `carina3122_cloudfront_allowed_methods_set_is_no_change_via_pipeline`:
/// the SAME order-reversed value-equal lists, but with the schema
/// declaring the list **ordered** (`AttributeType::list`, i.e.
/// `ordered: true`). This is exactly the stale-pin condition
/// (carina#3122 root cause): the pinned old awscc revision generated
/// `allowed_methods` as an ordered list, so carina-core compared the
/// elements positionally and reported a phantom change.
///
/// Asserting `Diff::Update` here pins that the schema's `ordered`
/// flag is the deciding factor — without this, the no-change test
/// above could keep passing even if the schema-typed ordered-list arm
/// regressed, because the no-schema fallback path always treats lists
/// as multisets. Together the two tests prove: ordered ⇒ Change,
/// unordered ⇒ NoChange, for identical value-equal/order-reversed
/// input.
#[test]
fn carina3122_cloudfront_allowed_methods_ordered_list_does_change_via_pipeline() {
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};
    use crate::value::{canonicalize_resources_with_schemas, canonicalize_states_with_schemas};

    let method_enum = |name: &str, values: &[&str]| AttributeType::StringEnum {
        name: name.to_string(),
        values: values.iter().map(|s| s.to_string()).collect(),
        namespace: Some("awscc.cloudfront.Distribution".to_string()),
        dsl_aliases: values
            .iter()
            .map(|s| (s.to_string(), s.to_lowercase()))
            .collect(),
    };
    // The only load-bearing difference for the `allowed_methods`
    // verdict vs the no-change test: `list` (ordered: true) instead of
    // `unordered_list`. (This test uses a trimmed shape — no
    // `cached_methods`, no read-back-default state block — since only
    // the `allowed_methods` ordering is under test here.)
    let default_cache_behavior = AttributeType::Struct {
        name: "DefaultCacheBehavior".to_string(),
        fields: vec![
            StructField::new(
                "allowed_methods",
                AttributeType::list(method_enum(
                    "AllowedMethods",
                    &["GET", "HEAD", "OPTIONS", "PUT", "PATCH", "POST", "DELETE"],
                )),
            )
            .with_provider_name("AllowedMethods"),
        ],
    };
    let distribution_config = AttributeType::Struct {
        name: "DistributionConfig".to_string(),
        fields: vec![
            StructField::new("default_cache_behavior", default_cache_behavior)
                .with_provider_name("DefaultCacheBehavior"),
        ],
    };
    let mut schema = ResourceSchema::new("cloudfront.Distribution");
    schema.attributes.insert(
        "distribution_config".to_string(),
        AttributeSchema::new("distribution_config", distribution_config),
    );
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert("awscc", schema.clone());

    let ns = |alias: &str| {
        Value::Concrete(ConcreteValue::String(format!(
            "awscc.cloudfront.Distribution.AllowedMethods.{alias}"
        )))
    };
    let list = |a: &str, b: &str| Value::Concrete(ConcreteValue::List(vec![ns(a), ns(b)]));
    let dcb = |a: &str, b: &str| {
        let mut m = IndexMap::new();
        m.insert("allowed_methods".to_string(), list(a, b));
        let mut dc = IndexMap::new();
        dc.insert(
            "default_cache_behavior".to_string(),
            Value::Concrete(ConcreteValue::Map(m)),
        );
        Value::Concrete(ConcreteValue::Map(dc))
    };

    let mut resources = vec![
        Resource::new("cloudfront.Distribution", "d1")
            .with_attribute("distribution_config", dcb("get", "head")),
    ];
    resources[0].id.provider = "awscc".to_string();
    canonicalize_resources_with_schemas(&mut resources, &registry);

    let mut state_attrs = HashMap::new();
    state_attrs.insert("distribution_config".to_string(), dcb("head", "get"));
    let mut id = ResourceId::new("cloudfront.Distribution", "d1");
    id.provider = "awscc".to_string();
    let mut states = std::collections::HashMap::new();
    let st = State::existing(id, state_attrs);
    states.insert(st.id.clone(), st);
    canonicalize_states_with_schemas(&mut states, &registry);

    let prev_explicit = crate::explicit::build_from_resource(&resources[0]);
    let current = states.into_values().next().unwrap();
    let result = diff(
        &resources[0],
        &current,
        None,
        Some(&prev_explicit),
        Some(&schema),
    );
    assert!(
        matches!(result, Diff::Update { .. }),
        "carina#3122: with an ORDERED list schema, the same \
         order-reversed value-equal lists MUST be a Change — this is \
         the stale-pin failure mode the no-change test guards \
         against. If this is NoChange the `ordered` flag is being \
         ignored and the contract is no longer pinned — got {result:?}"
    );
}
