use super::comparison::SavedAttr;
use super::{create_plan_for_tests as create_plan, *};

use crate::explicit::ExplicitFields;
use crate::resource::{CanonicalEnumValue, ConcreteValue, DeferredValue, Value, ValueString};
use crate::schema::{ResourceSchema, TypeIdentity};
use crate::value::value_to_json;
use indexmap::IndexMap;

fn resolved_ignored_resource(
    mut resource: Resource,
    schema: ResourceSchema,
    path: &[&str],
) -> (ResolvedResource, SchemaRegistry) {
    resource.directives.ignore_changes = vec![crate::resource::ParsedIgnorePath::new(
        path.iter()
            .map(|part| crate::resource::ParsedIgnorePathSegment::Attribute((*part).to_string()))
            .collect(),
    )];
    let mut registry = SchemaRegistry::new();
    registry.insert(
        resource.id.provider.clone(),
        schema.validate().expect("ignore_changes schema is valid"),
    );
    let resource = ResolvedResource::resolve_with_schemas(resource, &registry)
        .expect("ignore_changes path resolves");
    (resource, registry)
}

#[test]
fn ignored_top_level_readable_and_create_only_attributes_do_not_diff() {
    use crate::schema::AttributeSchema;

    let schema = ResourceSchema::new("Widget")
        .attribute(
            AttributeSchema::optional("function_version", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(AttributeSchema::optional("comment", AttributeType::string()).build());
    let desired = Resource::with_provider("test", "Widget", "alias", None)
        .with_attribute(
            "function_version",
            Value::Concrete(ConcreteValue::String("1".into())),
        )
        .with_attribute(
            "comment",
            Value::Concrete(ConcreteValue::String("same".into())),
        );
    let (desired, registry) = resolved_ignored_resource(desired, schema, &["function_version"]);
    let current = State::existing(
        desired.id.clone(),
        HashMap::from([
            (
                "function_version".to_string(),
                Value::Concrete(ConcreteValue::String("42".into())),
            ),
            (
                "comment".to_string(),
                Value::Concrete(ConcreteValue::String("same".into())),
            ),
        ]),
    );
    let saved = crate::provider::RawSavedAttrs::default().lift(&registry);
    let schema = registry
        .get_for(desired.as_resource())
        .expect("schema")
        .schema()
        .as_schema();

    assert!(matches!(
        diff(&desired, &current, &saved, None, Some(schema)),
        Diff::NoChange(_)
    ));
}

#[test]
fn ignored_readable_struct_leaf_does_not_hide_changed_sibling() {
    use crate::schema::{AttributeSchema, StructField};

    let schema = ResourceSchema::new("Widget").attribute(
        AttributeSchema::optional(
            "config",
            AttributeType::struct_(
                "Config",
                vec![
                    StructField::optional("function_version", AttributeType::string()).build(),
                    StructField::optional("memory", AttributeType::int()).build(),
                ],
            ),
        )
        .build(),
    );
    let config = |version: &str, memory: i64| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "function_version".to_string(),
                Value::Concrete(ConcreteValue::String(version.into())),
            ),
            (
                "memory".to_string(),
                Value::Concrete(ConcreteValue::Int(memory)),
            ),
        ])))
    };
    let desired = Resource::with_provider("test", "Widget", "function", None)
        .with_attribute("config", config("recorded", 256));
    let (desired, registry) =
        resolved_ignored_resource(desired, schema, &["config", "function_version"]);
    let current = State::existing(
        desired.id.clone(),
        HashMap::from([("config".to_string(), config("external", 128))]),
    );
    let saved = crate::provider::RawSavedAttrs::default().lift(&registry);
    let schema = registry
        .get_for(desired.as_resource())
        .expect("schema")
        .schema()
        .as_schema();

    let Diff::Update {
        changed_attributes, ..
    } = diff(&desired, &current, &saved, None, Some(schema))
    else {
        panic!("memory change must still produce an update")
    };
    assert_eq!(changed_attributes, ["config"]);
}

fn persisted_secret(encoded: &serde_json::Value) -> Value {
    crate::value::PersistedValue::from_serialized_json(encoded.clone())
        .expect("valid persisted secret")
        .decode()
        .expect("persisted secret decodes to a typed value")
}

#[test]
fn type_aware_int_float_coercion() {
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(42)),
        &Value::Concrete(ConcreteValue::Float(42.0)),
        Some(&AttributeType::float()),
        crate::schema::empty_defs_for_schema_walks(),
    ));
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Float(42.0)),
        &Value::Concrete(ConcreteValue::Int(42)),
        Some(&AttributeType::float()),
        crate::schema::empty_defs_for_schema_walks(),
    ));
    // Non-exact conversion should not be equal
    assert!(!type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(42)),
        &Value::Concrete(ConcreteValue::Float(42.5)),
        Some(&AttributeType::float()),
        crate::schema::empty_defs_for_schema_walks(),
    ));
    // Without type info, Int and Float are not equal
    assert!(!type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(42)),
        &Value::Concrete(ConcreteValue::Float(42.0)),
        None,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn differ_treats_saved_function_call_and_fresh_unavailable_as_equal() {
    let argument = Value::Concrete(ConcreteValue::String("X".into()));
    let saved = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![argument.clone().into()],
        source_line: 0.into(),
    });
    let fresh = Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![argument.into()],
        reason: crate::resource::UnavailableReason::from_builtin_error(
            "environment variable 'X' is not set".to_string(),
        ),
        source_line: 42.into(),
    });

    assert!(type_aware_equal(
        &saved,
        &fresh,
        Some(&AttributeType::string()),
        crate::schema::empty_defs_for_schema_walks(),
    ));
    assert!(type_aware_equal(
        &fresh,
        &saved,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn type_aware_int_float_coercion_for_int_type() {
    // Int type also allows coercion (e.g., provider returns Float for an Int field)
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(10)),
        &Value::Concrete(ConcreteValue::Float(10.0)),
        Some(&AttributeType::int()),
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn type_aware_list_with_inner_type() {
    let list_type = AttributeType::unordered_list(AttributeType::float());
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
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn type_aware_struct_per_field() {
    use crate::schema::StructField;

    let struct_type = AttributeType::struct_(
        "Config".to_string(),
        vec![
            StructField::optional("count", AttributeType::float()).build(),
            StructField::optional("name", AttributeType::string()).build(),
        ],
    );
    let a = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("count".to_string(), Value::Concrete(ConcreteValue::Int(5))),
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
    ])));
    let b = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "count".to_string(),
            Value::Concrete(ConcreteValue::Float(5.0)),
        ),
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
    ])));
    assert!(type_aware_equal(
        &a,
        &b,
        Some(&struct_type),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn type_aware_union_numeric() {
    let union_type = AttributeType::union(vec![AttributeType::int(), AttributeType::float()]);
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(7)),
        &Value::Concrete(ConcreteValue::Float(7.0)),
        Some(&union_type),
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn type_aware_custom_delegates_to_base() {
    let custom_type = AttributeType::refined_float_with_validator(
        TypeIdentity::bare("Port"),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
    );
    assert!(type_aware_equal(
        &Value::Concrete(ConcreteValue::Int(8080)),
        &Value::Concrete(ConcreteValue::Float(8080.0)),
        Some(&custom_type),
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn type_aware_diff_no_change_with_schema() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::optional("port", AttributeType::float()).build());

    let desired = Resource::new("test.resource", "test")
        .with_attribute("port", Value::Concrete(ConcreteValue::Int(443)));

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "port".to_string(),
        Value::Concrete(ConcreteValue::Float(443.0)),
    );
    let current = State::existing(
        ResourceId::with_identity("test.resource", "test"),
        current_attrs,
    );

    // Without schema: detects a change (Int != Float)
    let result = diff_test(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::Update { .. }),
        "Without schema, Int(443) != Float(443.0) should be Update, got {:?}",
        result
    );

    // With schema: no change (type-aware coercion)
    let result = diff_test(&desired, &current, None, None, Some(&schema));
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
    let struct_type = AttributeType::struct_(
        "ServerSideEncryptionRule".to_string(),
        vec![
            StructField::optional("bucket_key_enabled", AttributeType::bool()).build(),
            StructField::optional("sse_algorithm", AttributeType::string()).build(),
        ],
    );

    // Desired: only sse_algorithm specified (no bucket_key_enabled)
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "sse_algorithm".to_string(),
        Value::Concrete(ConcreteValue::String("AES256".into())),
    )])));

    // Current (from AWS): includes bucket_key_enabled: false as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "bucket_key_enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        ),
        (
            "sse_algorithm".to_string(),
            Value::Concrete(ConcreteValue::String("AES256".into())),
        ),
    ])));

    assert!(
        type_aware_equal(
            &desired,
            &current,
            Some(&struct_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Struct with extra default Bool(false) should be considered equal"
    );
}

#[test]
fn type_aware_struct_does_not_ignore_non_default_bool() {
    use crate::schema::StructField;

    let struct_type = AttributeType::struct_(
        "ServerSideEncryptionRule".to_string(),
        vec![
            StructField::optional("bucket_key_enabled", AttributeType::bool()).build(),
            StructField::optional("sse_algorithm", AttributeType::string()).build(),
        ],
    );

    // Desired: only sse_algorithm
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "sse_algorithm".to_string(),
        Value::Concrete(ConcreteValue::String("AES256".into())),
    )])));

    // Current: bucket_key_enabled is true (non-default) — should NOT be equal
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "bucket_key_enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "sse_algorithm".to_string(),
            Value::Concrete(ConcreteValue::String("AES256".into())),
        ),
    ])));

    assert!(
        !type_aware_equal(
            &desired,
            &current,
            Some(&struct_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Struct with non-default Bool(true) should NOT be considered equal"
    );
}

#[test]
fn type_aware_enum_namespaced_vs_raw() {
    // Enum with namespace
    let enum_type = AttributeType::enum_(
        crate::schema::enum_identity(
            "ServerSideEncryptionByDefaultSseAlgorithm",
            Some("awscc.s3.Bucket"),
        ),
        Some(vec![
            "aws:kms".to_string(),
            "AES256".to_string(),
            "aws:kms:dsse".to_string(),
        ]),
        vec![],
        None,
        None,
    );

    // Typed namespaced form vs provider/API string.
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::enum_identifier(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            &Value::Concrete(ConcreteValue::String("AES256".into())),
            Some(&enum_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Namespaced enum and raw value should be considered equal"
    );

    // Both in typed namespaced form.
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::enum_identifier(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            &Value::Concrete(ConcreteValue::enum_identifier(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            Some(&enum_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Both namespaced should be equal"
    );

    // Different values should not match
    assert!(
        !type_aware_equal(
            &Value::Concrete(ConcreteValue::enum_identifier(
                "awscc.s3.Bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256".to_string()
            )),
            &Value::Concrete(ConcreteValue::String("aws:kms".into())),
            Some(&enum_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Different enum values should not be equal"
    );
}

/// Regression for carina#3312: `Enum` attributes with a `to_dsl`
/// callback (e.g. `aws.AvailabilityZone`, where `to_dsl = |s|
/// s.replace('-', '_')`) must compare equal across the provider/API
/// `String("ap-northeast-1a")` and typed DSL `EnumIdentifier(...)` shapes.
#[test]
fn type_aware_enum_canonical_vs_namespaced_identifier() {
    let az_type = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["AvailabilityZone"], "ZoneName"),
        None,
        vec![],
        None,
        Some(crate::schema::DslTransform::HyphenToUnderscore),
    );

    // Provider-read AWS canonical form vs DSL fully-qualified identifier.
    // State side stores `String("ap-northeast-1a")`; the DSL parser emits a
    // typed `EnumIdentifier` for the fully-qualified source form.
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            &Value::Concrete(ConcreteValue::enum_identifier(
                "aws.AvailabilityZone.ap_northeast_1a"
            )),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "AWS-canonical hyphenated form must equal the fully-qualified DSL identifier"
    );
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::enum_identifier(
                "aws.AvailabilityZone.ap_northeast_1a"
            )),
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Equality must be symmetric"
    );

    // Same dotted DSL form on both sides.
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::enum_identifier(
                "aws.AvailabilityZone.ap_northeast_1a"
            )),
            &Value::Concrete(ConcreteValue::enum_identifier(
                "aws.AvailabilityZone.ap_northeast_1a"
            )),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Identical fully-qualified forms must be equal"
    );

    // Bare canonical form on both sides — identical strings, trivially equal.
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Identical AWS-canonical forms must be equal"
    );

    // Cross-shape: state String vs the parser's DSL EnumIdentifier.
    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            &Value::Concrete(ConcreteValue::enum_identifier(
                "aws.AvailabilityZone.ap_northeast_1a".to_string()
            )),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "AWS-canonical String must equal the matching EnumIdentifier shape"
    );

    // Different values must still produce a real diff (don't over-fold).
    assert!(
        !type_aware_equal(
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            &Value::Concrete(ConcreteValue::enum_identifier(
                "aws.AvailabilityZone.ap_northeast_1c"
            )),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Different enum values must not be folded as equal"
    );
}

#[test]
fn type_aware_enum_canonical_enum_value_compares_against_api_string() {
    let az_identity = TypeIdentity::new(Some("aws"), ["AvailabilityZone"], "ZoneName");
    let az_type = AttributeType::enum_(
        az_identity.clone(),
        None,
        vec![],
        None,
        Some(crate::schema::DslTransform::HyphenToUnderscore),
    );

    assert!(
        type_aware_equal(
            &Value::Concrete(ConcreteValue::CanonicalEnum(
                CanonicalEnumValue::new_for_test(az_identity.clone(), "ap-northeast-1a"),
            )),
            &Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "CanonicalEnum API value must compare equal to provider API text"
    );

    assert!(
        !type_aware_equal(
            &Value::Concrete(ConcreteValue::CanonicalEnum(
                CanonicalEnumValue::new_for_test(az_identity.clone(), "ap-northeast-1a"),
            )),
            &Value::Concrete(ConcreteValue::String(
                "aws.AvailabilityZone.ap_northeast_1a".into()
            )),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "a dotted String must remain literal provider/API text"
    );

    assert!(
        !type_aware_equal(
            &Value::Concrete(ConcreteValue::CanonicalEnum(
                CanonicalEnumValue::new_for_test(az_identity, "ap-northeast-1a"),
            )),
            &Value::Concrete(ConcreteValue::String(
                "aws.AvailabilityZone.ap_northeast_1c".into()
            )),
            Some(&az_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Different CanonicalEnum API values must not compare equal"
    );
}

#[test]
fn type_aware_struct_ignores_default_enum_empty() {
    use crate::schema::StructField;

    let struct_type = AttributeType::struct_(
        "Config".to_string(),
        vec![
            StructField::optional("name", AttributeType::string()).build(),
            StructField::optional(
                "status",
                AttributeType::enum_(
                    TypeIdentity::bare("Status"),
                    Some(vec!["Active".to_string(), "Inactive".to_string()]),
                    vec![],
                    None,
                    None,
                ),
            )
            .build(),
        ],
    );

    // Desired: only name specified
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    )])));

    // Current: includes status: "" as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        (
            "status".to_string(),
            Value::Concrete(ConcreteValue::String(String::new().into())),
        ),
    ])));

    assert!(
        type_aware_equal(
            &desired,
            &current,
            Some(&struct_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Struct with extra default Enum empty string should be considered equal"
    );
}

#[test]
fn type_aware_struct_ignores_default_custom_type() {
    use crate::schema::StructField;

    let struct_type = AttributeType::struct_(
        "Config".to_string(),
        vec![
            StructField::optional("name", AttributeType::string()).build(),
            StructField::optional(
                "port",
                AttributeType::refined_int_with_validator(
                    TypeIdentity::bare("Port"),
                    None,
                    crate::schema::legacy_validator(|_| Ok(())),
                ),
            )
            .build(),
        ],
    );

    // Desired: only name specified
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    )])));

    // Current: includes port: 0 as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        ("port".to_string(), Value::Concrete(ConcreteValue::Int(0))),
    ])));

    assert!(
        type_aware_equal(
            &desired,
            &current,
            Some(&struct_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Struct with extra default Custom(Int) zero should be considered equal"
    );
}

#[test]
fn type_aware_struct_ignores_default_nested_struct_empty() {
    use crate::schema::StructField;

    let struct_type = AttributeType::struct_(
        "Outer".to_string(),
        vec![
            StructField::optional("name", AttributeType::string()).build(),
            StructField::optional(
                "inner",
                AttributeType::struct_(
                    "Inner".to_string(),
                    vec![StructField::optional("value", AttributeType::string()).build()],
                ),
            )
            .build(),
        ],
    );

    // Desired: only name specified
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    )])));

    // Current: includes inner: {} as default
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        (
            "inner".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        ),
    ])));

    assert!(
        type_aware_equal(
            &desired,
            &current,
            Some(&struct_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Struct with extra default nested Struct empty map should be considered equal"
    );
}

#[test]
fn type_aware_ordered_list_detects_reorder() {
    // An ordered list (insertionOrder=true) should detect reordering as a change
    let ordered_list_type = AttributeType::list(AttributeType::string());

    // Same elements, different order
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".into())),
        Value::Concrete(ConcreteValue::String("b".into())),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("b".into())),
        Value::Concrete(ConcreteValue::String("a".into())),
    ]));

    assert!(
        !type_aware_equal(
            &a,
            &b,
            Some(&ordered_list_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Ordered list should detect reorder as NOT equal"
    );

    // Same elements, same order should still be equal
    let c = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".into())),
        Value::Concrete(ConcreteValue::String("b".into())),
    ]));
    assert!(
        type_aware_equal(
            &a,
            &c,
            Some(&ordered_list_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Ordered list with same order should be equal"
    );
}

#[test]
fn type_aware_unordered_list_ignores_reorder() {
    // An unordered list (insertionOrder=false) should treat reordering as no change
    let unordered_list_type = AttributeType::unordered_list(AttributeType::string());

    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".into())),
        Value::Concrete(ConcreteValue::String("b".into())),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("b".into())),
        Value::Concrete(ConcreteValue::String("a".into())),
    ]));

    assert!(
        type_aware_equal(
            &a,
            &b,
            Some(&unordered_list_type),
            crate::schema::empty_defs_for_schema_walks()
        ),
        "Unordered list should treat reorder as equal"
    );
}

#[test]
fn oidc_listener_with_matching_nested_write_only_provenance_converges() {
    use crate::provider::NestedWriteOnlyRecord;
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};

    let oidc_config = AttributeType::struct_(
        "AuthenticateOidcConfig",
        vec![
            StructField::optional("issuer", AttributeType::string()).build(),
            StructField::optional("client_secret", AttributeType::string())
                .write_only()
                .build(),
        ],
    );
    let schema = ResourceSchema::new("elasticloadbalancingv2.Listener")
        .attribute(
            AttributeSchema::optional(
                "default_actions",
                AttributeType::list(AttributeType::struct_(
                    "Action",
                    vec![
                        StructField::optional("type", AttributeType::string()).build(),
                        StructField::optional("authenticate_oidc_config", oidc_config).build(),
                    ],
                )),
            )
            .build(),
        )
        .attribute(AttributeSchema::optional("ssl_policy", AttributeType::string()).build());

    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("oidc-client-secret".into()),
    ))));
    let action = |client_secret: Option<Value>| {
        let mut config = IndexMap::from([(
            "issuer".to_string(),
            Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
        )]);
        if let Some(client_secret) = client_secret {
            config.insert("client_secret".to_string(), client_secret);
        }
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([
                (
                    "type".to_string(),
                    Value::Concrete(ConcreteValue::String("authenticate-oidc".into())),
                ),
                (
                    "authenticate_oidc_config".to_string(),
                    Value::Concrete(ConcreteValue::Map(config)),
                ),
            ])),
        )]))
    };
    let desired = HashMap::from([
        ("default_actions".to_string(), action(Some(secret))),
        (
            "ssl_policy".to_string(),
            Value::Concrete(ConcreteValue::String(
                "ELBSecurityPolicy-TLS13-1-2-2021-06".into(),
            )),
        ),
    ]);
    let current = HashMap::from([
        ("default_actions".to_string(), action(None)),
        (
            "ssl_policy".to_string(),
            Value::Concrete(ConcreteValue::String(
                "ELBSecurityPolicy-TLS13-1-2-2021-06".into(),
            )),
        ),
    ]);
    let mut nested_write_only = NestedWriteOnlyRecord::default();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["default_actions"], &desired_for_record, None)
        .expect("OIDC client secret is persistable");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "matching accepted client_secret provenance must converge: {changed:?}"
    );
}

fn oidc_listener_diff_fixture(
    client_secret: &str,
) -> (
    ResourceSchema,
    HashMap<String, Value>,
    HashMap<String, Value>,
) {
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};

    let schema = ResourceSchema::new("elasticloadbalancingv2.Listener")
        .attribute(
            AttributeSchema::optional(
                "default_actions",
                AttributeType::list(AttributeType::struct_(
                    "Action",
                    vec![
                        StructField::optional(
                            "authenticate_oidc_config",
                            AttributeType::struct_(
                                "AuthenticateOidcConfig",
                                vec![
                                    StructField::optional("issuer", AttributeType::string())
                                        .build(),
                                    StructField::optional("client_secret", AttributeType::string())
                                        .write_only()
                                        .build(),
                                ],
                            ),
                        )
                        .build(),
                    ],
                )),
            )
            .build(),
        )
        .attribute(AttributeSchema::optional("ssl_policy", AttributeType::string()).build());
    let oidc_action = |secret: Option<&str>| {
        let mut config = IndexMap::from([(
            "issuer".to_string(),
            Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
        )]);
        if let Some(secret) = secret {
            config.insert(
                "client_secret".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String(secret.into()),
                )))),
            );
        }
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "authenticate_oidc_config".to_string(),
                Value::Concrete(ConcreteValue::Map(config)),
            )])),
        )]))
    };
    let ssl_policy = Value::Concrete(ConcreteValue::String(
        "ELBSecurityPolicy-TLS13-1-2-2021-06".into(),
    ));
    (
        schema,
        HashMap::from([
            (
                "default_actions".to_string(),
                oidc_action(Some(client_secret)),
            ),
            ("ssl_policy".to_string(), ssl_policy.clone()),
        ]),
        HashMap::from([
            ("default_actions".to_string(), oidc_action(None)),
            ("ssl_policy".to_string(), ssl_policy),
        ]),
    )
}

#[test]
fn key_should_enter_patch_uses_matching_recorded_nested_write_only_provenance() {
    let (schema, desired, current) = oidc_listener_diff_fixture("client-secret");
    let mut nested_write_only = NestedWriteOnlyRecord::new();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["default_actions"], &desired_for_record, None)
        .expect("OIDC client secret is persistable");
    let recorded = nested_write_only
        .get("default_actions")
        .expect("matching leaf provenance was recorded");
    assert_eq!(recorded.len(), 1);

    let desired_value = desired
        .get("default_actions")
        .expect("fixture has desired default actions");
    let current_value = current
        .get("default_actions")
        .expect("fixture has provider-read default actions");
    let attr_type = &schema.attributes["default_actions"].attr_type;

    assert!(
        !key_should_enter_patch(
            "default_actions",
            Some(&schema),
            LeafProvenance::Recorded(Some(recorded)),
            AttrComparison {
                from: Some(current_value),
                to: desired_value,
                saved: None,
                type_info: Some(TypedAttr {
                    attr_type,
                    defs: &schema.defs,
                }),
            },
        ),
        "matching recorded provenance must suppress the provider-omitted leaf"
    );
}

#[test]
fn oidc_listener_secret_rotation_plans_update() {
    use crate::provider::NestedWriteOnlyRecord;

    let (schema, previous_desired, _current) = oidc_listener_diff_fixture("old-secret");
    let (_equivalent_schema, desired, current) = oidc_listener_diff_fixture("new-secret");
    let mut nested_write_only = NestedWriteOnlyRecord::default();
    let previous_for_record = previous_desired
        .clone()
        .into_iter()
        .collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["default_actions"], &previous_for_record, None)
        .unwrap();

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert_eq!(changed, ["default_actions"]);
}

#[test]
fn oidc_listener_missing_nested_write_only_record_plans_reconciliation_update() {
    let (schema, desired, current) = oidc_listener_diff_fixture("client-secret");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );

    assert_eq!(changed, ["default_actions"]);
}

#[test]
fn oidc_listener_without_write_only_leaf_needs_no_provenance() {
    let (schema, _desired_with_secret, current) = oidc_listener_diff_fixture("unused");
    let desired = current.clone();

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "an absent record is complete when the desired value has no write-only leaves: {changed:?}"
    );
}

fn secret_wrapped_oidc_listener_fixture(
    wrap_default_actions: bool,
) -> (
    ResourceSchema,
    HashMap<String, Value>,
    HashMap<String, Value>,
) {
    use crate::schema::{AttributeSchema, StructField};

    let schema = ResourceSchema::new("elasticloadbalancingv2.Listener").attribute(
        AttributeSchema::optional(
            "default_actions",
            AttributeType::list(AttributeType::struct_(
                "Action",
                vec![
                    StructField::optional(
                        "authenticate_oidc_config",
                        AttributeType::struct_(
                            "AuthenticateOidcConfig",
                            vec![
                                StructField::optional("issuer", AttributeType::string()).build(),
                                StructField::optional("client_secret", AttributeType::string())
                                    .write_only()
                                    .build(),
                            ],
                        ),
                    )
                    .build(),
                ],
            )),
        )
        .build(),
    );
    let issuer = || Value::Concrete(ConcreteValue::String("https://issuer.example.com".into()));
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("issuer".to_string(), issuer()),
        (
            "client_secret".to_string(),
            Value::Concrete(ConcreteValue::String("oidc-client-secret".into())),
        ),
    ])));
    let observable_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "issuer".to_string(),
        issuer(),
    )])));
    let action = |config| {
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "authenticate_oidc_config".to_string(),
                config,
            )])),
        )]))
    };
    let secret = |value| Value::Deferred(DeferredValue::Secret(Box::new(value)));

    let (desired_actions, current_actions) = if wrap_default_actions {
        let desired = secret(action(desired_config));
        let observable = secret(action(observable_config));
        let current = persisted_secret(&value_to_json(&observable).unwrap());
        (desired, current)
    } else {
        let desired = action(secret(desired_config));
        let observable = secret(observable_config);
        let current = action(persisted_secret(&value_to_json(&observable).unwrap()));
        (desired, current)
    };

    (
        schema,
        HashMap::from([("default_actions".to_string(), desired_actions)]),
        HashMap::from([("default_actions".to_string(), current_actions)]),
    )
}

#[test]
fn oidc_listener_with_secret_wrapped_config_converges() {
    use crate::provider::NestedWriteOnlyRecord;

    let (schema, desired, current) = secret_wrapped_oidc_listener_fixture(false);
    let mut nested_write_only = NestedWriteOnlyRecord::new();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["default_actions"], &desired_for_record, None)
        .expect("secret-wrapped config is persistable");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "a matching secret-wrapped OIDC config must converge: {changed:?}"
    );
}

#[test]
fn oidc_listener_with_secret_wrapped_action_list_converges() {
    use crate::provider::NestedWriteOnlyRecord;

    let (schema, desired, current) = secret_wrapped_oidc_listener_fixture(true);
    let mut nested_write_only = NestedWriteOnlyRecord::new();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["default_actions"], &desired_for_record, None)
        .expect("secret-wrapped action list is persistable");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "a matching secret-wrapped action list must converge: {changed:?}"
    );
}

#[test]
fn entirely_write_only_struct_absent_from_read_converges_with_matching_record() {
    use crate::provider::NestedWriteOnlyRecord;
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};

    let schema = ResourceSchema::new("test.Credential").attribute(
        AttributeSchema::optional(
            "creds",
            AttributeType::struct_(
                "Credentials",
                vec![
                    StructField::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .build(),
    );
    let desired = HashMap::from([(
        "creds".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("accepted-secret".into()),
            )))),
        )]))),
    )]);
    let current = HashMap::new();
    let mut nested_write_only = NestedWriteOnlyRecord::default();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["creds"], &desired_for_record, None)
        .expect("fixture credentials are persistable");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "an omitted attribute with no observable leaves must converge: {changed:?}"
    );
}

#[test]
fn omitted_read_with_empty_observable_string_list_converges() {
    use crate::provider::NestedWriteOnlyRecord;
    use crate::schema::{AttributeSchema, StructField};

    let schema = ResourceSchema::new("test.Credential").attribute(
        AttributeSchema::optional(
            "creds",
            AttributeType::struct_(
                "Credentials",
                vec![
                    StructField::optional("aliases", AttributeType::list(AttributeType::string()))
                        .build(),
                    StructField::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .build(),
    );
    let desired = HashMap::from([(
        "creds".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "aliases".to_string(),
                Value::Concrete(ConcreteValue::StringList(Vec::new())),
            ),
            (
                "password".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("accepted-secret".into()),
                )))),
            ),
        ]))),
    )]);
    let current = HashMap::new();
    let mut nested_write_only = NestedWriteOnlyRecord::new();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["creds"], &desired_for_record, None)
        .expect("fixture credentials are persistable");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "an omitted attribute with only an empty observable string list must converge: {changed:?}"
    );
}

#[test]
fn entirely_write_only_secret_wrapped_struct_absent_from_read_converges() {
    use crate::provider::NestedWriteOnlyRecord;
    use crate::schema::{AttributeSchema, StructField};

    let schema = ResourceSchema::new("test.Credential").attribute(
        AttributeSchema::optional(
            "creds",
            AttributeType::struct_(
                "Credentials",
                vec![
                    StructField::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .build(),
    );
    let desired = HashMap::from([(
        "creds".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("accepted-secret".into())),
            )])),
        )))),
    )]);
    let current = HashMap::new();
    let mut nested_write_only = NestedWriteOnlyRecord::new();
    let desired_for_record = desired.clone().into_iter().collect::<IndexMap<_, _>>();
    nested_write_only
        .record_sent(&schema, ["creds"], &desired_for_record, None)
        .expect("secret-wrapped credentials are persistable");

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &nested_write_only,
        None,
        Some(&schema),
    );

    assert!(
        changed.is_empty(),
        "a secret wrapper around an empty observable projection carries no provider content: {changed:?}"
    );
}

// --- Tests for absent write-only attributes in find_changed_attributes ---

#[test]
fn non_create_only_write_only_attr_in_desired_not_in_current_detects_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                .write_only()
                .build(),
        );

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);
    // CloudControl Read API does not return write-only attributes
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    )]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );
    assert!(
        changed.contains(&"ipv4_netmask_length".to_string()),
        "updatable write-only attribute absent from current should trigger a diff: {changed:?}"
    );
}

#[test]
fn create_only_write_only_attr_in_desired_not_in_current_has_no_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                .write_only()
                .create_only()
                .build(),
        );

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    )]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );
    assert!(
        changed.is_empty(),
        "create-only write-only attribute absent from current must not force replacement: {changed:?}"
    );
}

#[test]
fn write_only_attr_in_both_same_value_no_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                .write_only()
                .build(),
        );

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);
    let current = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );
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
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                .write_only()
                .build(),
        );

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(24)),
        ),
    ]);
    let current = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "ipv4_netmask_length".to_string(),
            Value::Concrete(ConcreteValue::Int(16)),
        ),
    ]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );
    assert!(
        changed.contains(&"ipv4_netmask_length".to_string()),
        "Write-only attribute with different value should trigger a diff"
    );
}

#[test]
fn non_write_only_attr_in_desired_not_in_current_detects_diff() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("enable_dns", AttributeType::bool()).build());

    let desired = HashMap::from([
        (
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        ),
        (
            "enable_dns".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ]);
    let current = HashMap::from([(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    )]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
    );
    assert!(
        changed.contains(&"enable_dns".to_string()),
        "Non-write-only attribute absent from current should trigger a diff"
    );
}

#[test]
fn secret_unchanged_same_hash() {
    use crate::value::value_to_json;

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    // Compute the hash that would be stored in state
    let hash_json = value_to_json(&secret_value).unwrap();
    // State has a typed hash, desired has the Secret wrapper.
    assert!(type_aware_equal(
        &secret_value,
        &persisted_secret(&hash_json),
        None,
        crate::schema::empty_defs_for_schema_walks(),
    ));
    // Reversed order should also work
    assert!(type_aware_equal(
        &persisted_secret(&hash_json),
        &secret_value,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn secret_changed_different_hash() {
    use crate::value::value_to_json;

    let old_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("old-password".into()),
    ))));
    let new_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("new-password".into()),
    ))));
    // Compute the hash for the OLD secret (stored in state)
    let old_hash_json = value_to_json(&old_secret).unwrap();
    // New desired vs old state hash should be different
    assert!(!type_aware_equal(
        &new_secret,
        &persisted_secret(&old_hash_json),
        None,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn secret_in_find_changed_attributes_no_change() {
    use crate::value::value_to_json;

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    let hash_json = value_to_json(&secret_value).unwrap();
    let desired = HashMap::from([("password".to_string(), secret_value)]);
    let current = HashMap::from([("password".to_string(), persisted_secret(&hash_json))]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        None,
    );
    assert!(
        changed.is_empty(),
        "Secret with matching hash should not show as changed, got: {:?}",
        changed
    );
}

#[test]
fn diagnostic_sensitive_string_matches_plain_state_without_diff() {
    let desired = HashMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
            "hunter2SECRET",
        ))),
    )]);
    let current = HashMap::from([(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("hunter2SECRET".into())),
    )]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        None,
    );
    assert!(
        changed.is_empty(),
        "marker must not create diff churn: {changed:?}"
    );
}

#[test]
fn secret_in_find_changed_attributes_changed() {
    use crate::value::value_to_json;

    let old_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("old-password".into()),
    ))));
    let new_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("new-password".into()),
    ))));
    let old_hash_json = value_to_json(&old_secret).unwrap();
    let desired = HashMap::from([("password".to_string(), new_secret)]);
    let current = HashMap::from([("password".to_string(), persisted_secret(&old_hash_json))]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        None,
    );
    assert!(
        changed.contains(&"password".to_string()),
        "Secret with different hash should show as changed"
    );
}

#[test]
fn key_should_enter_patch_saved_merge_preserves_unmanaged_nested_fields() {
    let attr_type = AttributeType::map(AttributeType::string());
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("new".into())),
        ),
        (
            "ManagedBy".to_string(),
            Value::Concrete(ConcreteValue::String("carina".into())),
        ),
    ])));
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("new".into())),
    )])));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("old".into())),
        ),
        (
            "ManagedBy".to_string(),
            Value::Concrete(ConcreteValue::String("carina".into())),
        ),
    ])));

    assert!(!key_should_enter_patch(
        "tags",
        None,
        LeafProvenance::DecidedByPlan { key_changed: false },
        AttrComparison {
            from: Some(&current),
            to: &desired,
            saved: Some(SavedAttr {
                views: crate::resource::SavedValueViews::same(&saved),
                authoring: &ExplicitFields::Unrecorded,
            }),
            type_info: Some(TypedAttr {
                attr_type: &attr_type,
                defs: crate::schema::empty_defs_for_schema_walks(),
            }),
        },
    ));
}

fn post_create_unknown() -> Value {
    Value::Deferred(DeferredValue::Unknown(
        crate::resource::ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::incomplete("producer read-back was incomplete"),
        ),
    ))
}

#[test]
fn nested_post_create_read_unknown_cannot_enter_a_patch() {
    let current = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("old.example".into())),
    )])));
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "Name".to_string(),
        post_create_unknown(),
    )])));

    assert!(
        !key_should_enter_patch(
            "tags",
            None,
            LeafProvenance::DecidedByPlan { key_changed: false },
            AttrComparison {
                from: Some(&current),
                to: &desired,
                saved: None,
                type_info: None,
            },
        ),
        "a nested incomplete post-create observation is not a provider payload"
    );
}

#[test]
fn dependent_nested_post_create_read_unknown_cannot_force_replacement() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let desired_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "Name".to_string(),
        post_create_unknown(),
    )])));
    let desired = Resource::new("test.Consumer", "dependent").with_attribute("tags", desired_tags);
    let current = State::existing(
        desired.id.clone(),
        HashMap::from([(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "Name".to_string(),
                Value::Concrete(ConcreteValue::String("old.example".into())),
            )]))),
        )]),
    );
    let schema = ResourceSchema::new("test.Consumer").attribute(
        AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
            .create_only()
            .build(),
    );

    let result = diff_test(&desired, &current, None, None, Some(&schema));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "dependent state must wait for the producer's next refresh, got {result:?}"
    );
}

#[test]
fn secret_in_map_no_change_when_hash_matches() {
    // Desired: tags map with a secret value
    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("super-secret".into()),
    ))));
    let desired_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        ("SecretTag".to_string(), secret_value.clone()),
    ])));

    // State: tags map with the secret hash (as stored by from_provider_state)
    let hash_json = value_to_json(&secret_value).unwrap();
    let state_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        ("SecretTag".to_string(), persisted_secret(&hash_json)),
    ])));

    let desired = HashMap::from([("tags".to_string(), desired_tags)]);
    let current = HashMap::from([("tags".to_string(), state_tags)]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        None,
    );
    assert!(
        changed.is_empty(),
        "Secret in map with matching hash should not show as changed, got: {:?}",
        changed
    );
}

#[test]
fn secret_with_embedded_salt_no_change_when_hash_matches() {
    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    let hash_json = value_to_json(&secret_value).unwrap();
    let desired = HashMap::from([("master_password".to_string(), secret_value)]);
    let current = HashMap::from([("master_password".to_string(), persisted_secret(&hash_json))]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        None,
    );
    assert!(
        changed.is_empty(),
        "Secret with matching embedded-salt digest should not show as changed, got: {:?}",
        changed
    );
}

#[test]
fn secret_with_embedded_salt_detects_change() {
    let old_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("old-password".into()),
    ))));
    let new_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("new-password".into()),
    ))));
    let hash_json = value_to_json(&old_secret).unwrap();
    let desired = HashMap::from([("master_password".to_string(), new_secret)]);
    let current = HashMap::from([("master_password".to_string(), persisted_secret(&hash_json))]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        None,
    );
    assert!(
        changed.contains(&"master_password".to_string()),
        "Secret with different value should show as changed"
    );
}

#[test]
fn equal_plaintexts_get_distinct_digests_that_both_verify() {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("shared-password".into()),
    ))));

    let hash1 = value_to_json(&secret).unwrap();
    let hash2 = value_to_json(&secret).unwrap();

    assert_ne!(
        hash1, hash2,
        "random salts must hide equality between equal plaintexts"
    );

    let desired = HashMap::from([("master_password".to_string(), secret)]);
    for hash in [hash1, hash2] {
        let current = HashMap::from([("master_password".to_string(), persisted_secret(&hash))]);
        assert!(
            find_changed_attributes(
                &desired,
                &current,
                None,
                &NestedWriteOnlyRecord::new(),
                None,
                None,
            )
            .is_empty(),
            "each independently salted digest must verify the plaintext",
        );
    }
}

#[test]
fn provider_null_fields_do_not_create_a_secret_diff() {
    let provider_json = serde_json::json!({ "a": "x", "b": null });
    let provider_value = crate::value::PersistedValue::from_serialized_json(provider_json).unwrap();
    let marker = crate::provider::hash_provider_json_as_secret(&provider_value, None)
        .expect("hash provider secret");
    let current = HashMap::from([("config".to_string(), persisted_secret(marker.as_json()))]);
    let desired = HashMap::from([(
        "config".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "a".to_string(),
                Value::Concrete(ConcreteValue::String("x".into())),
            )])),
        )))),
    )]);

    assert!(
        find_changed_attributes(
            &desired,
            &current,
            None,
            &NestedWriteOnlyRecord::new(),
            None,
            None,
        )
        .is_empty(),
        "provider-only null fields must not cause a phantom secret update",
    );
}

/// Regression test for #1249: Secret compared against plain-text state value
/// (as returned by provider.read in refresh=true mode) should match when
/// the inner values are equal.
#[test]
fn secret_matches_plain_text_state_value() {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    let plain = Value::Concrete(ConcreteValue::String("my-password".into()));
    assert!(
        type_aware_equal(
            &secret,
            &plain,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Secret should match plain-text state when inner values are equal"
    );
    assert!(
        type_aware_equal(
            &plain,
            &secret,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        ),
        "Plain-text state should match secret when inner values are equal"
    );
}

/// Secret with different inner value should NOT match plain-text state.
#[test]
fn secret_does_not_match_different_plain_text() {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    let plain = Value::Concrete(ConcreteValue::String("other-password".into()));
    assert!(
        !type_aware_equal(
            &secret,
            &plain,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        ),
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
    use crate::schema::{AttributeSchema, ResourceSchema};

    // Desired: tags map with a secret value (as written in .crn)
    let desired_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        (
            "SecretTag".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("super-secret-value".into()),
            )))),
        ),
    ])));

    // Current state from provider read (refresh=true): plain-text values
    let current_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        ),
        (
            "SecretTag".to_string(),
            Value::Concrete(ConcreteValue::String("super-secret-value".into())),
        ),
    ])));

    // Build schema with tags as Map(String)
    let schema = ResourceSchema::new("ec2.Vpc").attribute(
        AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
    );

    let desired = HashMap::from([("tags".to_string(), desired_tags)]);
    let current = HashMap::from([("tags".to_string(), current_tags)]);

    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        None,
        Some(&schema),
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
    AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::list(AttributeType::string()),
    ])
}

fn string_or_unordered_list_of_strings_type() -> AttributeType {
    AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::unordered_list(AttributeType::string()),
    ])
}

#[test]
fn type_aware_equal_self_cyclic_union_ref_uses_concrete_member() {
    let root = AttributeType::ref_("A");
    let defs = std::collections::BTreeMap::from([(
        "A".to_string(),
        AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::string()]),
    )]);
    let x = Value::Concrete(ConcreteValue::String("x".into()));
    let y = Value::Concrete(ConcreteValue::String("y".into()));

    assert!(type_aware_equal(&x, &x, Some(&root), &defs));
    assert!(!type_aware_equal(&x, &y, Some(&root), &defs));
}

#[test]
fn type_aware_equal_skips_cyclic_union_member_and_reaches_sibling() {
    let root = AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::string()]);
    let defs = std::collections::BTreeMap::from([(
        "A".to_string(),
        AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::int()]),
    )]);
    let value = Value::Concrete(ConcreteValue::String("x".into()));

    assert!(type_aware_equal(&value, &value, Some(&root), &defs));
}

#[test]
fn union_string_or_list_canonical_string_list_equal_to_self() {
    // Two `Value::Concrete(ConcreteValue::StringList)` values with identical contents must be
    // equal under the differ for the `Union[String, list(String)]`
    // type. Sanity check that the canonical form is comparable at all.
    let union = string_or_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()]));
    assert!(type_aware_equal(
        &a,
        &b,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn union_string_or_list_canonical_string_list_diff_on_different_content() {
    let union = string_or_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["repo:bar:*".into()]));
    assert!(!type_aware_equal(
        &a,
        &b,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn union_string_or_unordered_list_compares_canonical_string_list_as_multiset() {
    let union = string_or_unordered_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["b".into(), "a".into()]));

    assert!(type_aware_equal(
        &a,
        &b,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn unordered_list_of_strings_compares_string_list_as_multiset() {
    let list = AttributeType::unordered_list(AttributeType::string());
    let a = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["b".into(), "a".into()]));

    assert!(type_aware_equal(
        &a,
        &b,
        Some(&list),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn union_string_or_ordered_list_keeps_canonical_string_list_order_significant() {
    let union = string_or_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["b".into(), "a".into()]));

    assert!(!type_aware_equal(
        &a,
        &b,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn union_string_or_unordered_list_counts_canonical_string_list_multiplicity() {
    let union = string_or_unordered_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec![
        "a".into(),
        "a".into(),
        "b".into(),
    ]));
    let b = Value::Concrete(ConcreteValue::StringList(vec![
        "a".into(),
        "b".into(),
        "b".into(),
    ]));

    assert!(!type_aware_equal(
        &a,
        &b,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
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
    let scalar = Value::Concrete(ConcreteValue::String("repo:foo:*".into()));
    let canonical = Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()]));
    assert!(!type_aware_equal(
        &scalar,
        &canonical,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));

    let legacy_list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::String("repo:foo:*".into()),
    )]));
    assert!(!type_aware_equal(
        &legacy_list,
        &canonical,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn union_string_or_list_compares_string_list() {
    let union = string_or_list_of_strings_type();
    let a = Value::Concrete(ConcreteValue::StringList(vec!["x".into()]));
    let b = Value::Concrete(ConcreteValue::StringList(vec!["x".into()]));
    assert!(type_aware_equal(
        &a,
        &b,
        Some(&union),
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn into_plan_input_current_map_from_states_canonicalizes_string_or_list_state() {
    use crate::schema::{AttributeSchema, ResourceSchema};

    let schema = ResourceSchema::new("iam.policy")
        .attribute(AttributeSchema::optional("action", string_or_list_of_strings_type()).build());
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut desired = Resource::new("iam.policy", "assume-role-policy").with_attribute(
        "action",
        Value::Concrete(ConcreteValue::StringList(vec!["sts:AssumeRole".into()])),
    );
    desired.id.provider = "aws".to_string();
    let resources = vec![desired];

    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "action".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("sts:AssumeRole".into()),
        )])),
    );
    let mut id = ResourceId::with_identity("iam.policy", "assume-role-policy");
    id.provider = "aws".to_string();
    let mut states = HashMap::new();
    states.insert(id.clone(), State::existing(id, state_attrs));

    let plan_input_states =
        crate::resource::into_plan_input_current_map_from_states(states, &registry, &resources);
    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &plan_input_states,
        &HashMap::new(),
        &registry,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(
        plan.effects().is_empty(),
        "into_plan_input_current_map_from_states must canonicalize degraded List[String] state \
         to StringList before diffing; effects were {:?}",
        plan.effects()
    );
}

#[test]
fn into_plan_input_current_map_from_states_canonicalizes_separator_folded_enum_state_before_diffing()
 {
    use crate::schema::{AttributeSchema, ResourceSchema, enum_identity};

    let schema = ResourceSchema::new("acm.Certificate").attribute(
        AttributeSchema::optional(
            "key_algorithm",
            AttributeType::enum_(
                enum_identity("KeyAlgorithm", Some("aws.acm.Certificate")),
                Some(vec!["RSA_2048".to_string()]),
                vec![],
                None,
                None,
            ),
        )
        .create_only()
        .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut desired = Resource::new("acm.Certificate", "cert").with_attribute(
        "key_algorithm",
        Value::Concrete(ConcreteValue::enum_identifier("rsa_2048".to_string())),
    );
    desired.id.provider = "aws".to_string();
    let resources = vec![desired];

    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "key_algorithm".to_string(),
        Value::Concrete(ConcreteValue::String("RSA-2048".into())),
    );
    let mut id = ResourceId::with_identity("acm.Certificate", "cert");
    id.provider = "aws".to_string();
    let mut states = HashMap::new();
    states.insert(id.clone(), State::existing(id, state_attrs));

    let plan_input_states =
        crate::resource::into_plan_input_current_map_from_states(states, &registry, &resources);
    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &plan_input_states,
        &HashMap::new(),
        &registry,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(
        plan.effects().is_empty(),
        "separator-folded enum state must canonicalize to schema spelling before diffing; \
         effects were {:?}",
        plan.effects()
    );
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

    let principal = AttributeType::union(vec![
        AttributeType::struct_(
            "PrincipalStruct".to_string(),
            vec![
                StructField::optional(
                    "service",
                    AttributeType::union(vec![
                        AttributeType::string(),
                        AttributeType::list(AttributeType::string()),
                    ]),
                )
                .build(),
            ],
        ),
        AttributeType::string(),
    ]);
    let schema = ResourceSchema::new("iam.policy")
        .attribute(AttributeSchema::optional("principal", principal).build());
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert(
        "aws",
        (schema.clone())
            .validate()
            .expect("schema fixture must be valid"),
    );

    // Desired: user's bare scalar inside the Struct member.
    let mut desired_inner = IndexMap::new();
    desired_inner.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::String("cloudfront.amazonaws.com".into())),
    );
    let mut resources = vec![Resource::new("iam.policy", "p1").with_attribute(
        "principal",
        Value::Concrete(ConcreteValue::Map(desired_inner)),
    )];
    // Force the provider segment so the registry lookup ("aws") hits.
    resources[0].id.provider = "aws".to_string();
    canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

    // State: aws-read singleton list inside the Struct member.
    let mut state_inner = IndexMap::new();
    state_inner.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("cloudfront.amazonaws.com".into()),
        )])),
    );
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "principal".to_string(),
        Value::Concrete(ConcreteValue::Map(state_inner)),
    );
    let mut id = ResourceId::with_identity("iam.policy", "p1");
    id.provider = "aws".to_string();
    let mut states = std::collections::HashMap::new();
    let st = State::existing(id, state_attrs);
    states.insert(st.id.clone(), Current::from_state(st));
    canonicalize_states_with_schemas(&mut states, &registry);

    let current = states.into_values().next().unwrap();
    let current = match &current {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
        Current::MissingIdentifier(state) => state.state(),
    };
    let result = diff_test(&resources[0], current, None, None, Some(&schema));
    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3080: scalar (desired) vs singleton-list (state) under \
         Union[Struct,String] must be NoChange after the real \
         canonicalize pipeline — got {result:?}"
    );
}

/// carina#3740: persisted values merged into an authored container must pass
/// through the same structural canonicalization as desired and current state.
#[test]
fn carina3740_saved_nested_unauthored_union_scalar_is_no_change_via_pipeline() {
    use crate::provider::RawSavedAttrs;
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};
    use crate::value::{canonicalize_resources_with_schemas, canonicalize_states_with_schemas};

    let statement = AttributeType::struct_(
        "Statement".to_string(),
        vec![
            StructField::optional("effect", AttributeType::string()).build(),
            StructField::optional(
                "action",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )
            .build(),
        ],
    );
    let schema = ResourceSchema::new("iam.policy")
        .attribute(AttributeSchema::optional("statement", statement).build());
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "aws",
        (schema.clone())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let mut desired_statement = IndexMap::new();
    desired_statement.insert(
        "effect".to_string(),
        Value::Concrete(ConcreteValue::String("Allow".into())),
    );
    let mut resources = vec![Resource::new("iam.policy", "p1").with_attribute(
        "statement",
        Value::Concrete(ConcreteValue::Map(desired_statement)),
    )];
    resources[0].id.provider = "aws".to_string();
    canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

    let persisted_statement = || {
        IndexMap::from([
            (
                "effect".to_string(),
                Value::Concrete(ConcreteValue::String("Allow".into())),
            ),
            (
                "action".to_string(),
                Value::Concrete(ConcreteValue::String("s3:GetObject".into())),
            ),
        ])
    };
    let id = resources[0].id.clone();
    let mut states = HashMap::from([(
        id.clone(),
        Current::from_state(State::existing(
            id.clone(),
            HashMap::from([(
                "statement".to_string(),
                Value::Concrete(ConcreteValue::Map(persisted_statement())),
            )]),
        )),
    )]);
    canonicalize_states_with_schemas(&mut states, &registry);

    let saved_attrs = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
        id.clone(),
        HashMap::from([(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::Map(persisted_statement())),
        )]),
    )]))
    .lift(&registry);

    let current = match states.get(&id).unwrap() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
        Current::MissingIdentifier(state) => state.state(),
    };
    let result = diff_test(
        &resources[0],
        current,
        saved_attrs.get(&id),
        None,
        Some(&schema),
    );
    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3740: a saved scalar nested under an authored Struct must canonicalize before \
         merge_with_saved compares it with current state — got {result:?}"
    );
}

/// carina#3740: a persisted secret hash is an opaque scalar sentinel. Saved
/// reconciliation must not turn it into a list before `merge_with_saved`
/// carries the unauthored nested field into the effective desired value.
#[test]
fn carina3740_saved_nested_secret_hash_union_scalar_is_no_change() {
    use crate::provider::RawSavedAttrs;
    use crate::schema::{AttributeSchema, ResourceSchema, StructField};

    let config = AttributeType::struct_(
        "Config".to_string(),
        vec![
            StructField::optional("enabled", AttributeType::bool()).build(),
            StructField::optional(
                "password",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )
            .build(),
        ],
    );
    let mut schema = ResourceSchema::new("service.Widget");
    schema.attributes.insert(
        "config".to_string(),
        AttributeSchema::optional("config", config).build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "test",
        (schema.clone())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired_config = IndexMap::from([(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    )]);
    let mut desired = Resource::new("service.Widget", "widget").with_attribute(
        "config",
        Value::Concrete(ConcreteValue::Map(desired_config)),
    );
    desired.id.provider = "test".to_string();

    let secret_hash = persisted_secret(&serde_json::json!({
        "$secret": format!("argon2-legacy:73616c74:{}", "deadbeef".repeat(8)),
    }));
    let persisted_config = || {
        IndexMap::from([
            (
                "enabled".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
            ("password".to_string(), secret_hash.clone()),
        ])
    };
    let current = State::existing(
        desired.id.clone(),
        HashMap::from([(
            "config".to_string(),
            Value::Concrete(ConcreteValue::Map(persisted_config())),
        )]),
    );
    let saved_attrs = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
        desired.id.clone(),
        HashMap::from([(
            "config".to_string(),
            Value::Concrete(ConcreteValue::Map(persisted_config())),
        )]),
    )]))
    .lift(&registry);

    let result = diff_test(
        &desired,
        &current,
        saved_attrs.get(&desired.id),
        None,
        Some(&schema),
    );
    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3740: matching current and saved secret hashes nested under an authored Struct \
         must converge — got {result:?}"
    );
}

/// carina#3122 SHAPE A: a steady-state no-op plan on awscc CloudFront
/// `Distribution` must NOT report a change for `allowed_methods` /
/// `cached_methods`. Those are nested two Struct levels deep
/// (`distribution_config` → `default_cache_behavior` →
/// `allowed_methods`) and schema-typed `unordered_list(Enum)`.
///
/// Representative subset modeling the same axes as the live plan
/// against `carina-rs/infra registry/dev/registry` (the verbatim live
/// capture is `tests/carina3122_live_phantom.rs`). Both sides reach
/// the differ in their host-canonicalized form: authored DSL identifiers and
/// provider API strings both become `CanonicalEnum`. The values are identical
/// and only the element ORDER differs:
/// - State (`from`):   `["HEAD", "GET"]` — order head, get
/// - Desired (`to`):   `[AllowedMethods.get, AllowedMethods.head]` — order get, head
///
/// `allowed_methods` is schema-typed `unordered_list(Enum)`, so
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

    let method_enum = |name: &str, values: &[&str]| {
        AttributeType::enum_(
            crate::schema::enum_identity(name, Some("awscc.cloudfront.Distribution")),
            Some(values.iter().map(|s| s.to_string()).collect()),
            values
                .iter()
                .map(|s| (s.to_string(), s.to_lowercase().replace('-', "_")))
                .collect(),
            None,
            None,
        )
    };
    let default_cache_behavior = AttributeType::struct_(
        "DefaultCacheBehavior".to_string(),
        vec![
            StructField::optional(
                "allowed_methods",
                AttributeType::unordered_list(method_enum(
                    "AllowedMethods",
                    &["GET", "HEAD", "OPTIONS", "PUT", "PATCH", "POST", "DELETE"],
                )),
            )
            .with_provider_name("AllowedMethods")
            .build(),
            StructField::optional(
                "cached_methods",
                AttributeType::unordered_list(method_enum(
                    "CachedMethods",
                    &["GET", "HEAD", "OPTIONS"],
                )),
            )
            .with_provider_name("CachedMethods")
            .build(),
            // User-authored scalar fields, so the test also covers the
            // "authored field survives projection and compares equal"
            // path alongside the read-back-default stripping.
            StructField::optional("compress", AttributeType::bool())
                .with_provider_name("Compress")
                .build(),
            StructField::optional("target_origin_id", AttributeType::string())
                .with_provider_name("TargetOriginId")
                .build(),
            StructField::optional(
                "viewer_protocol_policy",
                method_enum(
                    "ViewerProtocolPolicy",
                    &["allow-all", "https-only", "redirect-to-https"],
                ),
            )
            .with_provider_name("ViewerProtocolPolicy")
            .build(),
        ],
    );
    let distribution_config = AttributeType::struct_(
        "DistributionConfig".to_string(),
        vec![
            StructField::optional("default_cache_behavior", default_cache_behavior)
                .with_provider_name("DefaultCacheBehavior")
                .build(),
        ],
    );
    let schema = ResourceSchema::new("cloudfront.Distribution")
        .attribute(AttributeSchema::optional("distribution_config", distribution_config).build());
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert(
        "awscc",
        (schema.clone())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired_method = |type_name: &str, alias: &str| {
        Value::Concrete(ConcreteValue::enum_identifier(format!(
            "awscc.cloudfront.Distribution.{type_name}.{alias}"
        )))
    };
    let desired_methods = |type_name: &str, a: &str, b: &str| {
        Value::Concrete(ConcreteValue::List(vec![
            desired_method(type_name, a),
            desired_method(type_name, b),
        ]))
    };
    let state_methods = |a: &str, b: &str| {
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String(a.to_ascii_uppercase().into())),
            Value::Concrete(ConcreteValue::String(b.to_ascii_uppercase().into())),
        ]))
    };

    // Desired (`to`): order get, head. Includes the user-authored
    // scalar fields so they survive `prev_explicit` projection and are
    // compared (equal) on both sides — distinct from the read-back
    // defaults below which are projected away.
    let s = |v: &str| Value::Concrete(ConcreteValue::String(v.to_string().into()));
    let mut desired_dcb = IndexMap::new();
    desired_dcb.insert(
        "allowed_methods".to_string(),
        desired_methods("AllowedMethods", "get", "head"),
    );
    desired_dcb.insert(
        "cached_methods".to_string(),
        desired_methods("CachedMethods", "get", "head"),
    );
    desired_dcb.insert(
        "compress".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    desired_dcb.insert("target_origin_id".to_string(), s("s3-origin"));
    desired_dcb.insert(
        "viewer_protocol_policy".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.cloudfront.Distribution.ViewerProtocolPolicy.redirect_to_https",
        )),
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
    canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

    // State (`from`): provider API spellings in the opposite order
    // (HEAD, GET), plus server-side read-back defaults the user never authored.
    // These exact keys/values are from the live plan `--json` diff of
    // `from` vs `to` (registry/dev/registry, state_serial 33). They
    // are present ONLY in `from` (state); `to` (desired) has just the
    // user-authored fields. Critically, several are NOT type-zero
    // (`http_version` is a non-empty Enum, `ipv6_enabled`/
    // `staging` are `true`), so `is_type_default` does NOT tolerate
    // them — they must instead be stripped by the `prev_explicit`
    // projection. This test exercises the real `diff` signature with
    // `prev_explicit` built from the desired resource (as the
    // carina-cli plan path does).
    let empty_list = || Value::Concrete(ConcreteValue::List(vec![]));
    let mut state_dcb = IndexMap::new();
    state_dcb.insert("allowed_methods".to_string(), state_methods("head", "get"));
    state_dcb.insert("cached_methods".to_string(), state_methods("head", "get"));
    // User-authored default_cache_behavior fields — identical on both
    // sides, so they survive projection and compare equal.
    state_dcb.insert(
        "compress".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    state_dcb.insert("target_origin_id".to_string(), s("s3-origin"));
    state_dcb.insert("viewer_protocol_policy".to_string(), s("redirect-to-https"));
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
    let saved_attrs = state_attrs.clone();
    let mut id = ResourceId::with_identity("cloudfront.Distribution", "d1");
    id.provider = "awscc".to_string();
    let mut states = std::collections::HashMap::new();
    let st = State::existing(id, state_attrs);
    states.insert(st.id.clone(), Current::from_state(st));
    canonicalize_states_with_schemas(&mut states, &registry);

    // The carina-cli plan path passes `prev_explicit` built from the
    // desired resource so server-side defaults are projected out of
    // the state side before comparison. Model that here.
    let prev_explicit = crate::explicit::build_from_resource(&resources[0]);

    let current = states.into_values().next().unwrap();
    let current = match &current {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
        Current::MissingIdentifier(state) => state.state(),
    };
    let result = diff_test(
        &resources[0],
        current,
        Some(&saved_attrs),
        Some(&prev_explicit),
        Some(&schema),
    );
    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3122 SHAPE A: a steady-state no-op plan must be \
         NoChange. State carries server read-back defaults the user \
         never authored (http_version, ipv6_enabled, …) plus \
         order-reversed unordered List<Enum> allowed_methods/\
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

    let method_enum = |name: &str, values: &[&str]| {
        AttributeType::enum_(
            crate::schema::enum_identity(name, Some("awscc.cloudfront.Distribution")),
            Some(values.iter().map(|s| s.to_string()).collect()),
            values
                .iter()
                .map(|s| (s.to_string(), s.to_lowercase().replace('-', "_")))
                .collect(),
            None,
            None,
        )
    };
    // The only load-bearing difference for the `allowed_methods`
    // verdict vs the no-change test: `list` (ordered: true) instead of
    // `unordered_list`. (This test uses a trimmed shape — no
    // `cached_methods`, no read-back-default state block — since only
    // the `allowed_methods` ordering is under test here.)
    let default_cache_behavior = AttributeType::struct_(
        "DefaultCacheBehavior".to_string(),
        vec![
            StructField::optional(
                "allowed_methods",
                AttributeType::list(method_enum(
                    "AllowedMethods",
                    &["GET", "HEAD", "OPTIONS", "PUT", "PATCH", "POST", "DELETE"],
                )),
            )
            .with_provider_name("AllowedMethods")
            .build(),
        ],
    );
    let distribution_config = AttributeType::struct_(
        "DistributionConfig".to_string(),
        vec![
            StructField::optional("default_cache_behavior", default_cache_behavior)
                .with_provider_name("DefaultCacheBehavior")
                .build(),
        ],
    );
    let schema = ResourceSchema::new("cloudfront.Distribution")
        .attribute(AttributeSchema::optional("distribution_config", distribution_config).build());
    let mut registry = crate::schema::SchemaRegistry::new();
    registry.insert(
        "awscc",
        (schema.clone())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired = |alias: &str| {
        Value::Concrete(ConcreteValue::enum_identifier(format!(
            "awscc.cloudfront.Distribution.AllowedMethods.{alias}"
        )))
    };
    let desired_list =
        |a: &str, b: &str| Value::Concrete(ConcreteValue::List(vec![desired(a), desired(b)]));
    let state_list = |a: &str, b: &str| {
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String(a.to_ascii_uppercase().into())),
            Value::Concrete(ConcreteValue::String(b.to_ascii_uppercase().into())),
        ]))
    };
    let dcb = |methods: Value| {
        let mut m = IndexMap::new();
        m.insert("allowed_methods".to_string(), methods);
        let mut dc = IndexMap::new();
        dc.insert(
            "default_cache_behavior".to_string(),
            Value::Concrete(ConcreteValue::Map(m)),
        );
        Value::Concrete(ConcreteValue::Map(dc))
    };

    let mut resources = vec![
        Resource::new("cloudfront.Distribution", "d1")
            .with_attribute("distribution_config", dcb(desired_list("get", "head"))),
    ];
    resources[0].id.provider = "awscc".to_string();
    canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "distribution_config".to_string(),
        dcb(state_list("head", "get")),
    );
    let mut id = ResourceId::with_identity("cloudfront.Distribution", "d1");
    id.provider = "awscc".to_string();
    let mut states = std::collections::HashMap::new();
    let st = State::existing(id, state_attrs);
    states.insert(st.id.clone(), Current::from_state(st));
    canonicalize_states_with_schemas(&mut states, &registry);

    let prev_explicit = crate::explicit::build_from_resource(&resources[0]);
    let current = states.into_values().next().unwrap();
    let current = match &current {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
        Current::MissingIdentifier(state) => state.state(),
    };
    let result = diff_test(
        &resources[0],
        current,
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

#[test]
fn find_changed_attributes_unrecorded_prev_explicit_is_no_change_when_values_match() {
    // carina#3280: a legacy state row whose authoring record was
    // destroyed by an older for-loop expansion path is signalled by
    // `ExplicitFields::Unrecorded` (emitted by the legacy-state
    // migration). The projection
    // must pass `current` through unchanged so the equality loop can
    // compare against `desired` directly — otherwise every attribute
    // gets flagged as changed and the plan surfaces a spurious
    // `forces replacement` for each one.
    use crate::explicit::ExplicitFields;
    use std::collections::HashMap;

    let desired: HashMap<String, Value> = HashMap::from([
        (
            "principal_type".to_string(),
            Value::Concrete(ConcreteValue::String("GROUP".into())),
        ),
        (
            "target_id".to_string(),
            Value::Concrete(ConcreteValue::String("123".into())),
        ),
    ]);
    let current = desired.clone();
    let prev_explicit = ExplicitFields::Unrecorded;
    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        Some(&prev_explicit),
        None,
    );
    assert!(
        changed.is_empty(),
        "carina#3280: `Unrecorded` is the \"no authoring record\" \
         degenerate case — desired == current must produce no changes, got {changed:?}"
    );
}

#[test]
fn find_changed_attributes_unrecorded_prev_explicit_still_detects_real_drift() {
    // carina#3280 sibling: confirm the `Unrecorded` pass-through
    // doesn't blind the differ to real drift. With no authoring
    // record the projection passes `current` through unchanged, so a
    // genuine value mismatch between `desired` and `current` must
    // still surface.
    use crate::explicit::ExplicitFields;
    use std::collections::HashMap;

    let desired: HashMap<String, Value> = HashMap::from([(
        "principal_type".to_string(),
        Value::Concrete(ConcreteValue::String("GROUP".into())),
    )]);
    let current: HashMap<String, Value> = HashMap::from([(
        "principal_type".to_string(),
        Value::Concrete(ConcreteValue::String("USER".into())),
    )]);
    let prev_explicit = ExplicitFields::Unrecorded;
    let changed = find_changed_attributes(
        &desired,
        &current,
        None,
        &NestedWriteOnlyRecord::new(),
        Some(&prev_explicit),
        None,
    );
    assert_eq!(
        changed,
        vec!["principal_type".to_string()],
        "real drift under `Unrecorded` must still be reported"
    );
}

#[test]
fn find_changed_attributes_keeps_legacy_unrecorded_list_element_still_in_desired() {
    let route_table_ids = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::String("rtb-1".into()),
    )]));
    let desired = HashMap::from([("route_table_ids".to_string(), route_table_ids.clone())]);
    let current = desired.clone();
    let saved = desired.clone();
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "route_table_ids".to_string(),
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::Unrecorded],
            },
        )]),
    };

    let changed = find_changed_attributes(
        &desired,
        &current,
        Some(&saved),
        &NestedWriteOnlyRecord::new(),
        Some(&prev_explicit),
        None,
    );

    assert!(
        changed.is_empty(),
        "a desired element is authored even when legacy state recorded it as Unrecorded: {changed:?}"
    );
}

#[test]
fn find_changed_attributes_keeps_nested_legacy_unrecorded_list_element_in_desired() {
    let network = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "route_table_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("rtb-1".into()),
        )])),
    )])));
    let desired = HashMap::from([("network".to_string(), network)]);
    let current = desired.clone();
    let saved = desired.clone();
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "network".to_string(),
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "route_table_ids".to_string(),
                    ExplicitFields::ListElements {
                        elements: vec![ExplicitFields::Unrecorded],
                    },
                )]),
            },
        )]),
    };

    let changed = find_changed_attributes(
        &desired,
        &current,
        Some(&saved),
        &NestedWriteOnlyRecord::new(),
        Some(&prev_explicit),
        None,
    );

    assert!(
        changed.is_empty(),
        "desired matching must reach nested ListElements records recursively: {changed:?}"
    );
}
