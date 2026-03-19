use super::*;

#[test]
fn type_aware_int_float_coercion() {
    assert!(type_aware_equal(
        &Value::Int(42),
        &Value::Float(42.0),
        Some(&AttributeType::Float),
    ));
    assert!(type_aware_equal(
        &Value::Float(42.0),
        &Value::Int(42),
        Some(&AttributeType::Float),
    ));
    // Non-exact conversion should not be equal
    assert!(!type_aware_equal(
        &Value::Int(42),
        &Value::Float(42.5),
        Some(&AttributeType::Float),
    ));
    // Without type info, Int and Float are not equal
    assert!(!type_aware_equal(
        &Value::Int(42),
        &Value::Float(42.0),
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
    let a = Value::Map(HashMap::from([
        ("count".to_string(), Value::Int(5)),
        ("name".to_string(), Value::String("test".to_string())),
    ]));
    let b = Value::Map(HashMap::from([
        ("count".to_string(), Value::Float(5.0)),
        ("name".to_string(), Value::String("test".to_string())),
    ]));
    assert!(type_aware_equal(&a, &b, Some(&struct_type)));
}

#[test]
fn type_aware_union_numeric() {
    let union_type = AttributeType::Union(vec![AttributeType::Int, AttributeType::Float]);
    assert!(type_aware_equal(
        &Value::Int(7),
        &Value::Float(7.0),
        Some(&union_type),
    ));
}

#[test]
fn type_aware_custom_delegates_to_base() {
    let custom_type = AttributeType::Custom {
        name: "Port".to_string(),
        base: Box::new(AttributeType::Float),
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert!(type_aware_equal(
        &Value::Int(8080),
        &Value::Float(8080.0),
        Some(&custom_type),
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
    let desired = Value::Map(HashMap::from([(
        "sse_algorithm".to_string(),
        Value::String("AES256".to_string()),
    )]));

    // Current (from AWS): includes bucket_key_enabled: false as default
    let current = Value::Map(HashMap::from([
        ("bucket_key_enabled".to_string(), Value::Bool(false)),
        (
            "sse_algorithm".to_string(),
            Value::String("AES256".to_string()),
        ),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type)),
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
    let desired = Value::Map(HashMap::from([(
        "sse_algorithm".to_string(),
        Value::String("AES256".to_string()),
    )]));

    // Current: bucket_key_enabled is true (non-default) — should NOT be equal
    let current = Value::Map(HashMap::from([
        ("bucket_key_enabled".to_string(), Value::Bool(true)),
        (
            "sse_algorithm".to_string(),
            Value::String("AES256".to_string()),
        ),
    ]));

    assert!(
        !type_aware_equal(&desired, &current, Some(&struct_type)),
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
        namespace: Some("awscc.s3.bucket".to_string()),
        to_dsl: None,
    };

    // Namespaced form vs raw string
    assert!(
        type_aware_equal(
            &Value::String(
                "awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            &Value::String("AES256".to_string()),
            Some(&enum_type),
        ),
        "Namespaced enum and raw value should be considered equal"
    );

    // Both in namespaced form
    assert!(
        type_aware_equal(
            &Value::String(
                "awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            &Value::String(
                "awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            Some(&enum_type),
        ),
        "Both namespaced should be equal"
    );

    // Different values should not match
    assert!(
        !type_aware_equal(
            &Value::String(
                "awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            ),
            &Value::String("aws:kms".to_string()),
            Some(&enum_type),
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
    let desired = Value::Map(HashMap::from([(
        "name".to_string(),
        Value::String("test".to_string()),
    )]));

    // Current: includes status: "" as default
    let current = Value::Map(HashMap::from([
        ("name".to_string(), Value::String("test".to_string())),
        ("status".to_string(), Value::String(String::new())),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type)),
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
                    name: "Port".to_string(),
                    base: Box::new(AttributeType::Int),
                    validate: |_| Ok(()),
                    namespace: None,
                    to_dsl: None,
                },
            ),
        ],
    };

    // Desired: only name specified
    let desired = Value::Map(HashMap::from([(
        "name".to_string(),
        Value::String("test".to_string()),
    )]));

    // Current: includes port: 0 as default
    let current = Value::Map(HashMap::from([
        ("name".to_string(), Value::String("test".to_string())),
        ("port".to_string(), Value::Int(0)),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type)),
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
    let desired = Value::Map(HashMap::from([(
        "name".to_string(),
        Value::String("test".to_string()),
    )]));

    // Current: includes inner: {} as default
    let current = Value::Map(HashMap::from([
        ("name".to_string(), Value::String("test".to_string())),
        ("inner".to_string(), Value::Map(HashMap::new())),
    ]));

    assert!(
        type_aware_equal(&desired, &current, Some(&struct_type)),
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
        !type_aware_equal(&a, &b, Some(&ordered_list_type)),
        "Ordered list should detect reorder as NOT equal"
    );

    // Same elements, same order should still be equal
    let c = Value::List(vec![
        Value::String("a".to_string()),
        Value::String("b".to_string()),
    ]);
    assert!(
        type_aware_equal(&a, &c, Some(&ordered_list_type)),
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
        type_aware_equal(&a, &b, Some(&unordered_list_type)),
        "Unordered list should treat reorder as equal"
    );
}
