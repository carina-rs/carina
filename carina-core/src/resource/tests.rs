use super::*;
use crate::explicit::ExplicitFields;
use crate::parser::SourceSpan;

fn merge_with_saved(
    desired: &Value,
    saved: SavedValueViews<'_>,
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Value {
    super::merge_with_saved(
        desired,
        saved,
        prior_explicit,
        attr_type,
        defs,
        &crate::differ::exact_comparison_context_for_tests(),
    )
}

fn pair_list_elements<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Vec<Option<usize>>
where
    D: std::borrow::Borrow<Value>,
    S: std::borrow::Borrow<Value>,
{
    super::pair_list_elements(
        desired,
        saved,
        attr_type,
        defs,
        &crate::differ::exact_comparison_context_for_tests(),
    )
}

fn pair_list_elements_with_uniqueness<D, S>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Vec<ListElementPairing>
where
    D: std::borrow::Borrow<Value>,
    S: std::borrow::Borrow<Value>,
{
    super::pair_list_elements_with_uniqueness(
        desired,
        saved,
        attr_type,
        defs,
        &crate::differ::exact_comparison_context_for_tests(),
    )
}

fn similarity_score(
    a: &Value,
    b: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> usize {
    super::similarity_score(
        a,
        b,
        attr_type,
        defs,
        &crate::differ::exact_comparison_context_for_tests(),
    )
}

fn merge_lists(
    desired: &[Value],
    projected_saved: &[Value],
    raw_saved: &[Value],
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Vec<Value> {
    super::merge_lists(
        desired,
        projected_saved,
        raw_saved,
        prior_explicit,
        attr_type,
        defs,
        &crate::differ::exact_comparison_context_for_tests(),
    )
}

#[test]
fn module_argument_source_location_is_not_value_data() {
    let located = Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
        name: "r".to_string(),
        source_line: (17).into(),
        diagnostic_name: None,
    }));
    let same_operand_other_line =
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ModuleArgument {
            name: "r".to_string(),
            source_line: (23).into(),
            diagnostic_name: None,
        }));

    assert_eq!(located, same_operand_other_line);
    let encoded = serde_json::to_string(&located).expect("serialize parser placeholder");
    assert!(
        !encoded.contains("source_line") && !encoded.contains("17"),
        "diagnostic location leaked into Value serialization: {encoded}",
    );
}

#[test]
fn function_call_source_location_is_not_value_data() {
    let call_at = |line: usize| {
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![Value::Concrete(ConcreteValue::String("value".into())).into()],
            source_line: line.into(),
        })
    };
    let located = call_at(17);

    assert_eq!(located, call_at(23));
    let encoded = serde_json::to_string(&located).expect("serialize deferred call");
    assert!(
        !encoded.contains("source_line") && !encoded.contains("17"),
        "diagnostic location leaked into Value serialization: {encoded}",
    );
    let decoded: Value = serde_json::from_str(&encoded).expect("deserialize deferred call");
    let Value::Deferred(DeferredValue::FunctionCall { source_line, .. }) = decoded else {
        panic!("expected a deferred function call");
    };
    assert_eq!(source_line.get(), None);
}

#[test]
fn deferred_function_call_deserialization_rejects_non_builtin_names() {
    let builtin = DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![Value::Concrete(ConcreteValue::String("value".into())).into()],
        source_line: DiagnosticSourceLine::synthetic(),
    };
    let encoded = serde_json::to_string(&builtin).expect("serialize deferred builtin call");
    let invalid = encoded.replace("upper", "user_helper");

    let error = serde_json::from_str::<DeferredValue>(&invalid)
        .expect_err("a deferred builtin call cannot carry a user-function name");
    assert!(
        error.to_string().contains("user_helper"),
        "unexpected deserialization error: {error}",
    );
}

#[test]
fn partial_read_marker_deserialization_rejects_empty_missing_attributes() {
    let json = r#"{
        "pending": {
            "reason": {
                "kind": "incomplete",
                "detail": "provider omitted every computed attribute"
            }
        },
        "missing_attributes": []
    }"#;

    let error = serde_json::from_str::<PartialReadMarker>(json)
        .expect_err("an empty partial-read marker must not be representable");

    assert!(
        error
            .to_string()
            .contains("partial-read marker must contain at least one missing attribute"),
        "unexpected deserialization error: {error}",
    );
}

#[test]
fn deferred_access_serde_round_trips() {
    let original = apply_postfix_access(
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::lookup,
            args: vec![Value::resource_ref("source", "values", vec![]).into()],
            source_line: (0).into(),
        }),
        PathSegment::Subscript {
            index: Subscript::Str { key: "name".into() },
        },
        0,
        AccessMissPolicy::Error,
    )
    .unwrap();

    let json = serde_json::to_string(&original).expect("serialize deferred access");
    let decoded: Value = serde_json::from_str(&json).expect("deserialize deferred access");

    assert_eq!(decoded, original);
}

#[test]
fn canonicalizing_an_access_rebuilds_after_interpolation_collapses() {
    let value = apply_postfix_access(
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("selected".into()),
        ])),
        PathSegment::Field {
            name: "identifier".to_string(),
        },
        0,
        AccessMissPolicy::Error,
    )
    .expect("the interpolation is deferred until canonicalization");
    let original = value.clone();

    assert_eq!(
        value.canonicalize(),
        original,
        "a base that canonicalizes to an invalid concrete shape must retain the sealed original, \
         not leave Access wrapped around the concrete string",
    );
}

#[test]
fn explicit_legacy_digest_verification_distinguishes_matching_and_mismatched_hashes() {
    let plaintext = br#""legacy-constant-time""#;
    let salt = b"carina:mock.test.resource:legacy:comment";
    let mut output = [0_u8; RedactedSecretHash::HASH_LEN];
    legacy_argon2()
        .hash_password_into(plaintext, salt, &mut output)
        .expect("hash legacy fixture");
    let marker = format!(
        "{}{}",
        RedactedSecretHash::LEGACY_MARKER_PREFIX,
        hex::encode(output)
    );
    let matching = RedactedSecretHash::upgrade_raw_legacy_marker_for_state_row(
        &marker,
        "mock.test.resource",
        "legacy",
        "comment",
    )
    .expect("legacy salt is representable")
    .expect("upgrade legacy fixture");
    assert!(matching.verify(plaintext));
    assert_eq!(
        matching.encoded_digest(),
        format!(
            "argon2-legacy:{}:{}",
            hex::encode(salt),
            hex::encode(output),
        ),
        "the self-contained marker stores only the row salt and digest",
    );

    output[0] ^= 1;
    let mismatched_marker = format!(
        "{}{}",
        RedactedSecretHash::LEGACY_MARKER_PREFIX,
        hex::encode(output)
    );
    let mismatched = RedactedSecretHash::upgrade_raw_legacy_marker_for_state_row(
        &mismatched_marker,
        "mock.test.resource",
        "legacy",
        "comment",
    )
    .expect("legacy salt is representable")
    .expect("upgrade mismatched legacy fixture");
    assert!(!mismatched.verify(plaintext));
}

#[test]
fn diagnostic_sensitive_string_is_runtime_equivalent_but_formatting_safe() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let plain = ValueString::plain("hunter2SECRET");
    let marked = ValueString::diagnostic_sensitive("hunter2SECRET");

    assert_eq!(marked, plain);

    let hash = |value: &ValueString| {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    };
    assert_eq!(hash(&marked), hash(&plain));
    assert_eq!(
        serde_json::to_string(&marked).unwrap(),
        r#""hunter2SECRET""#
    );
    assert_eq!(
        serde_json::to_string(&marked).unwrap(),
        serde_json::to_string(&plain).unwrap()
    );

    assert_eq!(marked.diagnostic_literal(), None);
    assert!(!format!("{marked:?}").contains("hunter2SECRET"));
    assert_eq!(marked.expose_plaintext(), "hunter2SECRET");
}

#[test]
fn secret_debug_never_formats_the_inner_plaintext() {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("hunter2SECRET".into()),
    ))));

    let rendered = format!("{secret:?}");

    assert!(rendered.contains("Secret((secret))"), "rendered={rendered}");
    assert!(!rendered.contains("hunter2SECRET"), "rendered={rendered}");
}

#[test]
fn interpolation_inherits_diagnostic_sensitivity_from_string_expression() {
    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::Concrete(ConcreteValue::String(
            ValueString::diagnostic_sensitive("hunter2SECRET"),
        ))),
    ]))
    .canonicalize();

    assert_eq!(crate::ValueDescription::of(&value).to_string(), "string");
    assert_eq!(crate::value::format_value(&value), "'prefix-hunter2SECRET'");
    assert_eq!(
        crate::value::value_to_json(&value).unwrap(),
        serde_json::json!("prefix-hunter2SECRET")
    );
}

#[test]
fn value_serde_round_trip() {
    let values = vec![
        Value::Concrete(ConcreteValue::String("hello".into())),
        Value::Concrete(ConcreteValue::Int(42)),
        Value::Concrete(ConcreteValue::Float(2.5)),
        Value::Concrete(ConcreteValue::Float(-0.5)),
        Value::Concrete(ConcreteValue::Bool(true)),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::Int(1)),
        ])),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "key".to_string(),
                Value::Concrete(ConcreteValue::String("val".into())),
            ),
            ("num".to_string(), Value::Concrete(ConcreteValue::Int(10))),
        ]))),
        Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
        Value::Concrete(ConcreteValue::String("dedicated".into())),
        Value::Concrete(ConcreteValue::String("InstanceTenancy.dedicated".into())),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(Value::resource_ref(
                "vpc".to_string(),
                "id".to_string(),
                vec![],
            )),
            InterpolationPart::Literal("-suffix".into()),
        ])),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Concrete(ConcreteValue::String("-".into())).into(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("a".into())),
                    Value::Concrete(ConcreteValue::String("b".into())),
                ]))
                .into(),
            ],
            source_line: (0).into(),
        }),
    ];

    for value in values {
        let json = serde_json::to_string(&value).unwrap();
        let deserialized: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value, deserialized, "Round-trip failed for {:?}", value);
    }
}

#[test]
fn resource_id_serde_round_trip() {
    let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "main-vpc", None);
    let json = serde_json::to_string(&id).unwrap();
    let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, deserialized);
}

#[test]
fn resource_identity_rejects_empty_string() {
    let err = serde_json::from_str::<ResourceIdentity>(r#""""#).unwrap_err();
    assert!(
        err.to_string()
            .contains("resource identity cannot be empty")
    );
}

#[test]
fn resource_identity_try_from_rejects_empty_runtime_string() {
    let error = ResourceIdentity::try_from(String::new()).unwrap_err();
    assert_eq!(error.to_string(), "resource identity cannot be empty");
}

#[test]
fn resource_identity_try_from_accepts_nonempty_runtime_string() {
    let identity = ResourceIdentity::try_from("runtime-identity".to_string()).unwrap();
    assert_eq!(identity.as_str(), "runtime-identity");
}

#[test]
fn resource_identity_accepts_nonempty_literal() {
    let identity = ResourceIdentity::from("literal-identity");
    assert_eq!(identity.as_str(), "literal-identity");
}

#[test]
fn distinct_pending_resource_ids_are_unequal_and_hash_separately() {
    let first = ResourceId::pending_with_provider("aws", "s3.Bucket", None);
    let second = ResourceId::pending_with_provider("aws", "s3.Bucket", None);

    assert_ne!(first, second);
    assert_eq!(std::collections::HashSet::from([first, second]).len(), 2);
}

#[test]
fn cloning_pending_resource_id_preserves_identity() {
    let id = ResourceId::pending_with_provider("aws", "s3.Bucket", None);
    assert_eq!(id, id.clone());
}

#[test]
fn instantiating_pending_resource_refreshes_token() {
    let template = Resource::pending_with_provider("aws", "s3.Bucket", None);
    let instance = template.instantiate();

    assert_ne!(template.id, instance.id);
    assert_eq!(template.attributes, instance.attributes);
}

#[test]
fn resolved_resource_id_equality_is_unchanged() {
    let first = ResourceId::with_provider_identity("aws", "s3.Bucket", "logs", None);
    let second = ResourceId::with_provider_identity("aws", "s3.Bucket", "logs", None);
    assert_eq!(first, second);
}

#[test]
fn with_provider_identity_maps_nonempty_to_some() {
    let id = ResourceId::with_provider_identity("aws", "s3.Bucket", "my-bucket", None);
    assert_eq!(id.identity_str(), Some("my-bucket"));
}

#[test]
fn pending_resource_id_and_resource_reject_serialization() {
    let id = ResourceId::pending_with_provider("aws", "ec2.Subnet", None);
    let resource = Resource::pending_with_provider("aws", "ec2.Subnet", None);

    assert!(serde_json::to_string(&id).is_err());
    assert!(serde_json::to_string(&resource).is_err());
}

#[test]
fn resource_id_identity_serde_round_trips_as_string() {
    let id = ResourceId::with_provider_identity("aws", "ec2.Subnet", "my-subnet", None);
    let json = serde_json::to_string(&id).unwrap();
    assert!(json.contains("\"identity\":\"my-subnet\""), "got: {json}");
    assert!(
        !json.contains("\"name\""),
        "resolved JSON shape changed: {json}"
    );
    let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, deserialized);
    assert_eq!(deserialized.identity_str(), Some("my-subnet"));
}

#[test]
fn resource_id_deserializes_legacy_name_alias() {
    let legacy = r#"{"provider":"aws","resource_type":"ec2.Subnet","name":"my-subnet"}"#;
    let deserialized: ResourceId = serde_json::from_str(legacy).unwrap();
    assert_eq!(deserialized.identity_str(), Some("my-subnet"));
}

#[test]
fn resource_id_missing_identity_mints_pending_token() {
    let json = r#"{"provider":"aws","resource_type":"ec2.Subnet"}"#;
    let first: ResourceId = serde_json::from_str(json).unwrap();
    let second: ResourceId = serde_json::from_str(json).unwrap();

    assert_eq!(first.identity_str(), None);
    assert_eq!(second.identity_str(), None);
    assert_ne!(first, second);
}

#[test]
#[should_panic(expected = "ResolvedResourceId requires identity")]
fn resolved_resource_id_new_panics_without_identity() {
    ResolvedResourceId::new(ResourceId::pending_with_provider("aws", "s3.Bucket", None));
}

#[test]
fn resolved_resource_id_try_new_returns_none_without_identity() {
    let id = ResourceId::pending_with_provider("aws", "s3.Bucket", None);
    assert!(ResolvedResourceId::try_new(id).is_none());
}

#[test]
fn resolved_resource_id_try_new_returns_some_with_identity() {
    let id = ResourceId::with_provider_identity("aws", "s3.Bucket", "logs", None);
    let resolved = ResolvedResourceId::try_new(id).expect("identity is present");
    assert_eq!(resolved.identity_str(), "logs");
}

#[test]
fn resolved_resource_id_identity_returns_resolved_identity() {
    let resolved = ResolvedResourceId::new(ResourceId::with_provider_identity(
        "aws",
        "s3.Bucket",
        "logs",
        None,
    ));

    assert_eq!(resolved.identity(), &ResourceIdentity::new("logs"));
}

#[test]
fn resolved_resource_id_serde_round_trips_as_resource_id() {
    let resolved = ResolvedResourceId::new(ResourceId::with_provider_identity(
        "aws",
        "s3.Bucket",
        "logs",
        Some("primary".to_string()),
    ));
    let json = serde_json::to_string(&resolved).unwrap();
    let decoded: ResolvedResourceId = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, resolved);
    assert_eq!(decoded.identity_str(), "logs");
}

#[test]
#[should_panic(expected = "ResolvedResource requires identity")]
fn resolved_resource_new_panics_without_identity() {
    ResolvedResource::new(Resource::pending("s3.Bucket"));
}

#[test]
fn resolved_resource_try_new_returns_none_without_identity() {
    assert!(ResolvedResource::try_new(Resource::pending("s3.Bucket")).is_none());
}

#[test]
fn resolved_resource_try_new_returns_some_with_identity() {
    let resolved =
        ResolvedResource::try_new(Resource::new("s3.Bucket", "logs")).expect("identity is present");
    assert_eq!(resolved.id.identity_str(), Some("logs"));
    assert_eq!(resolved.as_inner().id.identity_str(), Some("logs"));
    assert_eq!(
        resolved.clone().into_inner().id.identity_str(),
        Some("logs")
    );
}

#[test]
fn resolved_resource_serde_round_trips_as_resource() {
    let resolved = ResolvedResource::new(Resource::with_provider(
        "aws",
        "s3.Bucket",
        "logs",
        Some("primary".to_string()),
    ));
    let json = serde_json::to_string(&resolved).unwrap();
    let decoded = serde_json::from_str::<SavedResolvedResource>(&json).unwrap();
    assert_eq!(decoded, SavedResolvedResource::from(resolved));
    assert_eq!(decoded.id.identity_str(), Some("logs"));
}

#[test]
fn resolved_resource_deserialize_rejects_absent_identity() {
    let mut value = serde_json::to_value(Resource::new("s3.Bucket", "logs")).unwrap();
    value["id"].as_object_mut().unwrap().remove("identity");
    let err = serde_json::from_value::<SavedResolvedResource>(value).unwrap_err();
    assert!(err.to_string().contains("identity is required"));
}

#[test]
#[should_panic(expected = "ResolvedDataSource requires identity")]
fn resolved_data_source_new_panics_without_identity() {
    ResolvedDataSource::new(DataSource::pending("aws_ami"));
}

#[test]
fn resolved_data_source_try_new_returns_none_without_identity() {
    assert!(ResolvedDataSource::try_new(DataSource::pending("aws_ami")).is_none());
}

#[test]
fn resolved_data_source_try_new_returns_some_with_identity() {
    let resolved =
        ResolvedDataSource::try_new(DataSource::new("aws_ami", "ubuntu")).expect("identity");
    assert_eq!(resolved.id.identity_str(), Some("ubuntu"));
    assert_eq!(resolved.as_inner().id.identity_str(), Some("ubuntu"));
    assert_eq!(
        resolved.clone().into_inner().id.identity_str(),
        Some("ubuntu")
    );
}

#[test]
fn resolved_data_source_serde_round_trips_as_data_source() {
    let resolved = ResolvedDataSource::new(DataSource::with_provider(
        "aws",
        "ami",
        "ubuntu",
        Some("primary".to_string()),
    ));
    let json = serde_json::to_string(&resolved).unwrap();
    let decoded = serde_json::from_str::<SavedResolvedDataSource>(&json).unwrap();
    assert_eq!(decoded, SavedResolvedDataSource::from(resolved));
    assert_eq!(decoded.id.identity_str(), Some("ubuntu"));
}

#[test]
fn resolved_data_source_deserialize_rejects_absent_identity() {
    let mut value = serde_json::to_value(DataSource::new("aws_ami", "ubuntu")).unwrap();
    value["id"].as_object_mut().unwrap().remove("identity");
    let err = serde_json::from_value::<SavedResolvedDataSource>(value).unwrap_err();
    assert!(err.to_string().contains("identity is required"));
}

#[test]
fn resource_id_rejects_legacy_empty_name() {
    let legacy = r#"{"provider":"aws","resource_type":"ec2.Subnet","name":""}"#;
    assert!(
        serde_json::from_str::<ResourceId>(legacy).is_err(),
        "empty legacy name must fail — identity cannot be empty"
    );
}

#[test]
fn resource_id_rejects_null_identity() {
    let json = r#"{"provider":"aws","resource_type":"ec2.Subnet","identity":null}"#;
    assert!(
        serde_json::from_str::<ResourceId>(json).is_err(),
        "explicit null identity must not be interpreted as pending"
    );
}

/// The AC test from #2225: lookups keyed by `ResourceId` must remain
/// valid across the name-resolution pass. This is achieved by ensuring
/// that the parser starts with `Pending`, then any rename to `Bound`
/// produces a stable identifier.
/// We assert that two different mutation paths produce equal IDs.
#[test]
fn resource_id_rename_pending_to_bound() {
    let mut id = ResourceId::pending_with_provider("aws", "ec2.Subnet", None);
    // The post-pass assigns the extracted identity.
    id.set_identity(ResourceIdentity::new("app-subnet".to_string()));
    assert_eq!(id.identity_str(), Some("app-subnet"));
    // After renaming, the same string can produce an equal ResourceId
    // from any other code path (e.g. building a key for a sibling map).
    let constructed = ResourceId::with_provider_identity("aws", "ec2.Subnet", "app-subnet", None);
    assert_eq!(id, constructed);
}

#[test]
fn state_serde_round_trip() {
    let mut attrs = HashMap::new();
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    attrs.insert(
        "versioning".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );

    let state = State::existing(
        ResourceId::with_provider_identity("aws", "s3.Bucket", "my-bucket", None),
        attrs,
    )
    .with_identifier("my-bucket");

    let json = serde_json::to_string(&state).unwrap();
    let deserialized: State = serde_json::from_str(&json).unwrap();
    assert_eq!(state, deserialized);
}

#[test]
fn directives_serde_with_create_before_destroy() {
    let config = Directives {
        create_before_destroy: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: Directives = serde_json::from_str(&json).unwrap();
    assert_eq!(config, deserialized);
    assert!(deserialized.create_before_destroy);
}

#[test]
fn directives_serde_default() {
    // Old JSON without all fields should deserialize with defaults
    let json = r#"{"create_before_destroy":true}"#;
    let config: Directives = serde_json::from_str(json).unwrap();
    assert!(config.create_before_destroy);
    assert!(!config.force_delete);
    assert!(!config.prevent_destroy);
}

#[test]
fn directives_depends_on_defaults_to_empty() {
    let d = Directives::default();
    assert_eq!(d.depends_on, Vec::<String>::new());
}

#[test]
fn directives_depends_on_round_trips_serde() {
    let d = Directives {
        depends_on: vec!["role".to_string(), "key".to_string()],
        ..Directives::default()
    };
    let json = serde_json::to_string(&d).unwrap();
    let back: Directives = serde_json::from_str(&json).unwrap();
    assert_eq!(back.depends_on, d.depends_on);
}

#[test]
fn directives_serialises_without_depends_on_when_empty() {
    // Avoid state-file noise: an empty depends_on must not appear as
    // `"depends_on": []` in serialised JSON, otherwise every legacy
    // state file will gain a meaningless key on first re-write.
    let d = Directives::default();
    let json = serde_json::to_string(&d).unwrap();
    assert!(
        !json.contains("depends_on"),
        "empty depends_on should be skipped in serialisation, got {}",
        json
    );
}

#[test]
fn directives_depends_on_deserialises_from_legacy_json_without_field() {
    let legacy =
        r#"{ "force_delete": false, "create_before_destroy": false, "prevent_destroy": false }"#;
    let d: Directives = serde_json::from_str(legacy).unwrap();
    assert_eq!(d.depends_on, Vec::<String>::new());
}

#[test]
fn directives_with_force_delete() {
    let config = Directives {
        force_delete: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: Directives = serde_json::from_str(&json).unwrap();
    assert!(deserialized.force_delete);
    assert!(!deserialized.create_before_destroy);
    assert!(!deserialized.prevent_destroy);
}

#[test]
fn semantically_equal_lists_same_order() {
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
        Value::Concrete(ConcreteValue::Int(3)),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
        Value::Concrete(ConcreteValue::Int(3)),
    ]));
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_different_order() {
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(3)),
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
        Value::Concrete(ConcreteValue::Int(3)),
    ]));
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_different_content() {
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(3)),
    ]));
    assert!(!a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_different_lengths() {
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
        Value::Concrete(ConcreteValue::Int(3)),
    ]));
    assert!(!a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_with_duplicates() {
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
        Value::Concrete(ConcreteValue::Int(1)),
    ]));
    assert!(a.semantically_equal(&b));

    // Different multiplicities should not be equal
    let c = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    let d = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    assert!(!c.semantically_equal(&d));
}

#[test]
fn semantically_equal_empty_lists() {
    let a = Value::Concrete(ConcreteValue::List(vec![]));
    let b = Value::Concrete(ConcreteValue::List(vec![]));
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_of_maps_different_order() {
    let mut map1 = IndexMap::new();
    map1.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(80)));
    map1.insert(
        "protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );

    let mut map2 = IndexMap::new();
    map2.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(443)));
    map2.insert(
        "protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );

    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(map1.clone())),
        Value::Concrete(ConcreteValue::Map(map2.clone())),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(map2)),
        Value::Concrete(ConcreteValue::Map(map1)),
    ]));
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_of_strings() {
    let a = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("b".into())),
        Value::Concrete(ConcreteValue::String("a".into())),
    ]));
    let b = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("a".into())),
        Value::Concrete(ConcreteValue::String("b".into())),
    ]));
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_non_list_values() {
    // Non-list values should use regular equality
    assert!(
        Value::Concrete(ConcreteValue::Int(42))
            .semantically_equal(&Value::Concrete(ConcreteValue::Int(42)))
    );
    assert!(
        !Value::Concrete(ConcreteValue::Int(42))
            .semantically_equal(&Value::Concrete(ConcreteValue::Int(43)))
    );
    assert!(
        Value::Concrete(ConcreteValue::String("hello".into()))
            .semantically_equal(&Value::Concrete(ConcreteValue::String("hello".into())))
    );
    assert!(
        Value::Concrete(ConcreteValue::Bool(true))
            .semantically_equal(&Value::Concrete(ConcreteValue::Bool(true)))
    );
}

#[test]
fn semantically_equal_nested_lists() {
    // Lists inside maps are compared order-insensitively via recursive semantically_equal
    let mut map1 = IndexMap::new();
    map1.insert(
        "ports".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(80)),
            Value::Concrete(ConcreteValue::Int(443)),
        ])),
    );

    let mut map2 = IndexMap::new();
    map2.insert(
        "ports".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(443)),
            Value::Concrete(ConcreteValue::Int(80)),
        ])),
    );

    let a = Value::Concrete(ConcreteValue::Map(map1));
    let b = Value::Concrete(ConcreteValue::Map(map2));
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_maps_different_keys() {
    let mut map1 = IndexMap::new();
    map1.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));

    let mut map2 = IndexMap::new();
    map2.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(1)));

    assert!(
        !Value::Concrete(ConcreteValue::Map(map1))
            .semantically_equal(&Value::Concrete(ConcreteValue::Map(map2)))
    );
}

#[test]
fn semantically_equal_maps_different_sizes() {
    let mut map1 = IndexMap::new();
    map1.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));

    let mut map2 = IndexMap::new();
    map2.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    map2.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(2)));

    assert!(
        !Value::Concrete(ConcreteValue::Map(map1))
            .semantically_equal(&Value::Concrete(ConcreteValue::Map(map2)))
    );
}

#[test]
fn merge_with_saved_map_fills_extra_keys() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::Concrete(ConcreteValue::String("ip-name".into())),
        ),
        (
            "a_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ])));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::Concrete(ConcreteValue::String("ip-name".into())),
        ),
        (
            "a_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "aaaa_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        ),
    ])));

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    let expected = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::Concrete(ConcreteValue::String("ip-name".into())),
        ),
        (
            "a_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "aaaa_record".to_string(),
            Value::Concrete(ConcreteValue::Bool(false)),
        ),
    ])));
    assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
}

#[test]
fn merge_with_saved_desired_wins() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "a".to_string(),
        Value::Concrete(ConcreteValue::Int(10)),
    )])));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("a".to_string(), Value::Concrete(ConcreteValue::Int(5))),
        ("b".to_string(), Value::Concrete(ConcreteValue::Int(20))),
    ])));

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    let expected = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("a".to_string(), Value::Concrete(ConcreteValue::Int(10))),
        ("b".to_string(), Value::Concrete(ConcreteValue::Int(20))),
    ])));
    assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
}

#[test]
fn merge_with_saved_list_of_maps() {
    let desired = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(80)),
        )])),
    )]));
    let saved = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "protocol".to_string(),
                Value::Concrete(ConcreteValue::String("tcp".into())),
            ),
        ])),
    )]));

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    let expected = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "protocol".to_string(),
                Value::Concrete(ConcreteValue::String("tcp".into())),
            ),
        ])),
    )]));
    assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
}

#[test]
fn merge_with_saved_non_map() {
    let desired = Value::Concrete(ConcreteValue::String("hello".into()));
    let saved = Value::Concrete(ConcreteValue::String("world".into()));
    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    assert_eq!(
        merged,
        Value::Concrete(ConcreteValue::String("hello".into()))
    );

    let desired = Value::Concrete(ConcreteValue::Int(42));
    let saved = Value::Concrete(ConcreteValue::Int(99));
    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    assert_eq!(merged, Value::Concrete(ConcreteValue::Int(42)));
}

#[test]
fn merge_with_saved_does_not_reinject_authored_removed_key() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "web_acl_id".to_string(),
        Value::Concrete(ConcreteValue::String("arn:aws:wafv2:acl/test".into())),
    )])));
    let prior = ExplicitFields::Struct {
        children: HashMap::from([("web_acl_id".to_string(), ExplicitFields::Leaf)]),
    };

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &prior,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );

    assert_eq!(merged, Value::Concrete(ConcreteValue::Map(IndexMap::new())));
}

#[test]
fn merge_with_saved_reinjects_unauthored_saved_key() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "server_default".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    )])));
    let prior = ExplicitFields::Struct {
        children: HashMap::new(),
    };

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &prior,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );

    assert_eq!(merged, saved);
}

#[test]
fn merge_with_saved_reinjects_saved_key_with_unrecorded_child() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "previous_value".to_string(),
        Value::Concrete(ConcreteValue::Int(42)),
    )])));
    let prior = ExplicitFields::Struct {
        children: HashMap::from([("previous_value".to_string(), ExplicitFields::Unrecorded)]),
    };

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &prior,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );

    assert_eq!(merged, saved);
}

#[test]
fn merge_with_saved_unrecorded_reinjects_all_saved_keys() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "previous_value".to_string(),
        Value::Concrete(ConcreteValue::Int(42)),
    )])));

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );

    assert_eq!(merged, saved);
}

#[test]
fn merge_with_saved_applies_authoring_recursively_to_maps_and_lists() {
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "settings".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "nested".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "kept".to_string(),
                    Value::Concrete(ConcreteValue::Int(1)),
                )]))),
            )]))),
        ),
        (
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([(
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("primary".into())),
                )])),
            )])),
        ),
    ])));
    let saved = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "settings".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "nested".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("kept".to_string(), Value::Concrete(ConcreteValue::Int(1))),
                    (
                        "removed".to_string(),
                        Value::Concrete(ConcreteValue::Int(2)),
                    ),
                    (
                        "server_default".to_string(),
                        Value::Concrete(ConcreteValue::Int(3)),
                    ),
                ]))),
            )]))),
        ),
        (
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([
                    (
                        "name".to_string(),
                        Value::Concrete(ConcreteValue::String("primary".into())),
                    ),
                    (
                        "removed".to_string(),
                        Value::Concrete(ConcreteValue::Bool(true)),
                    ),
                    (
                        "server_default".to_string(),
                        Value::Concrete(ConcreteValue::Bool(false)),
                    ),
                ])),
            )])),
        ),
    ])));
    let prior = ExplicitFields::Struct {
        children: HashMap::from([
            (
                "settings".to_string(),
                ExplicitFields::Struct {
                    children: HashMap::from([(
                        "nested".to_string(),
                        ExplicitFields::Struct {
                            children: HashMap::from([
                                ("kept".to_string(), ExplicitFields::Leaf),
                                ("removed".to_string(), ExplicitFields::Leaf),
                            ]),
                        },
                    )]),
                },
            ),
            (
                "items".to_string(),
                ExplicitFields::List {
                    element: Box::new(ExplicitFields::Struct {
                        children: HashMap::from([
                            ("name".to_string(), ExplicitFields::Leaf),
                            ("removed".to_string(), ExplicitFields::Leaf),
                        ]),
                    }),
                },
            ),
        ]),
    };

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &prior,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    let expected = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "settings".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "nested".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("kept".to_string(), Value::Concrete(ConcreteValue::Int(1))),
                    (
                        "server_default".to_string(),
                        Value::Concrete(ConcreteValue::Int(3)),
                    ),
                ]))),
            )]))),
        ),
        (
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([
                    (
                        "name".to_string(),
                        Value::Concrete(ConcreteValue::String("primary".into())),
                    ),
                    (
                        "removed".to_string(),
                        Value::Concrete(ConcreteValue::Bool(true)),
                    ),
                    (
                        "server_default".to_string(),
                        Value::Concrete(ConcreteValue::Bool(false)),
                    ),
                ])),
            )])),
        ),
    ])));
    assert_eq!(merged, expected);
}

#[test]
fn merge_with_saved_does_not_apply_union_authoring_to_individual_list_elements() {
    let desired = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".into())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        )]))),
    ]));
    let saved = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".into())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(443))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("provider-default".into())),
            ),
        ]))),
    ]));
    let prior = ExplicitFields::List {
        element: Box::new(ExplicitFields::Struct {
            children: HashMap::from([
                ("port".to_string(), ExplicitFields::Leaf),
                ("description".to_string(), ExplicitFields::Leaf),
            ]),
        }),
    };

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &prior,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );

    assert_eq!(merged, saved);
}

#[test]
fn merge_with_saved_list_elements_drops_only_paired_authored_field_in_both_paths() {
    let rule = |port, description: Option<&str>| {
        let mut fields = IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(port)),
        )]);
        if let Some(description) = description {
            fields.insert(
                "description".to_string(),
                Value::Concrete(ConcreteValue::String(description.to_string().into())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let authored = |description| ExplicitFields::Struct {
        children: if description {
            HashMap::from([
                ("port".to_string(), ExplicitFields::Leaf),
                ("description".to_string(), ExplicitFields::Leaf),
            ])
        } else {
            HashMap::from([("port".to_string(), ExplicitFields::Leaf)])
        },
    };

    for stored_len in [2, HASH_THRESHOLD] {
        let mut desired = vec![rule(80, None), rule(443, None)];
        let mut saved = vec![rule(443, Some("provider-default")), rule(80, Some("web"))];
        let mut elements = vec![authored(false), authored(true)];
        for offset in 0..stored_len - 2 {
            let port = 1_000 + offset as i64;
            desired.push(rule(port, None));
            saved.push(rule(port, None));
            elements.push(authored(false));
        }
        let prior = ExplicitFields::ListElements { elements };

        let merged = merge_with_saved(
            &Value::Concrete(ConcreteValue::List(desired.clone())),
            SavedValueViews::same(&Value::Concrete(ConcreteValue::List(saved))),
            &prior,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        );
        desired[1] = rule(443, Some("provider-default"));

        assert_eq!(
            merged,
            Value::Concrete(ConcreteValue::List(desired)),
            "stored list length {stored_len} must use its paired authoring record"
        );
    }
}

#[test]
fn merge_with_saved_requires_raw_projected_corroboration_in_both_pairing_paths() {
    let cfg = |with_timeout: bool| {
        let mut fields = IndexMap::from([(
            "retries".to_string(),
            Value::Concrete(ConcreteValue::Int(3)),
        )]);
        if with_timeout {
            fields.insert(
                "timeout".to_string(),
                Value::Concrete(ConcreteValue::Int(30)),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let rule = |with_description: bool, with_timeout: bool| {
        let mut fields = IndexMap::from([("cfg".to_string(), cfg(with_timeout))]);
        if with_description {
            fields.insert(
                "desc".to_string(),
                Value::Concrete(ConcreteValue::String(String::new().into())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let authored_rule = ExplicitFields::Struct {
        children: HashMap::from([
            (
                "cfg".to_string(),
                ExplicitFields::Struct {
                    children: HashMap::from([("retries".to_string(), ExplicitFields::Leaf)]),
                },
            ),
            ("desc".to_string(), ExplicitFields::Leaf),
        ]),
    };

    for saved_len in [2, HASH_THRESHOLD] {
        let mut desired = vec![rule(false, false), rule(true, false)];
        let mut projected_saved = vec![rule(true, false), rule(true, false)];
        let mut raw_saved = vec![rule(true, true), rule(true, true)];
        let mut elements = vec![authored_rule.clone(), ExplicitFields::Unrecorded];
        for offset in 0..saved_len - 2 {
            let filler = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "id".to_string(),
                Value::Concrete(ConcreteValue::Int(1_000 + offset as i64)),
            )])));
            desired.push(filler.clone());
            projected_saved.push(filler.clone());
            raw_saved.push(filler);
            elements.push(ExplicitFields::Struct {
                children: HashMap::from([("id".to_string(), ExplicitFields::Leaf)]),
            });
        }

        let projected_pairs = pair_list_elements(
            &desired,
            &projected_saved,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        );
        let raw_pairs = pair_list_elements(
            &desired,
            &raw_saved,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(projected_pairs[0], Some(0));
        assert_eq!(raw_pairs[0], None);

        let desired = Value::Concrete(ConcreteValue::List(desired));
        let projected_saved = Value::Concrete(ConcreteValue::List(projected_saved));
        let raw_saved = Value::Concrete(ConcreteValue::List(raw_saved));
        let merged = merge_with_saved(
            &desired,
            SavedValueViews::new(&projected_saved, &raw_saved),
            &ExplicitFields::ListElements { elements },
            None,
            crate::schema::empty_defs_for_schema_walks(),
        );
        let Value::Concrete(ConcreteValue::List(merged)) = merged else {
            panic!("expected merged list");
        };
        let Value::Concrete(ConcreteValue::Map(first)) = &merged[0] else {
            panic!("expected first merged map");
        };
        assert_eq!(
            first.get("desc"),
            Some(&Value::Concrete(ConcreteValue::String(
                String::new().into()
            ))),
            "saved length {saved_len} must degrade divergent attribution to Unrecorded"
        );
    }
}

#[test]
fn pair_list_elements_commits_best_scores_before_lower_scores() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let map = |entries: &[(&str, &str)]| {
        Value::Concrete(ConcreteValue::Map(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_string(), string(value)))
                .collect(),
        ))
    };
    for saved_len in [1, HASH_THRESHOLD] {
        let mut saved = vec![map(&[
            ("sid", "ManagePublishStackRoles"),
            ("effect", "Allow"),
            ("action", "iam:CreateRole"),
        ])];
        let mut desired = vec![
            map(&[
                ("sid", "ECRRepository"),
                ("effect", "Allow"),
                ("action", "ecr:CreateRepository"),
            ]),
            map(&[
                ("sid", "ManagePublishStackRoles"),
                ("effect", "Allow"),
                ("action", "iam:DeleteRole"),
            ]),
        ];
        for index in 1..saved_len {
            let filler = map(&[("sid", &format!("filler-{index}"))]);
            saved.push(filler.clone());
            desired.push(filler);
        }

        let pairs = pair_list_elements(
            &desired,
            &saved,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(
            &pairs[..2],
            &[None, Some(0)],
            "saved length {saved_len} must commit the score-2 pair first"
        );
    }
}

#[test]
fn pair_list_elements_preserves_stable_indices_for_indistinguishable_peers_in_both_paths() {
    for len in [2, HASH_THRESHOLD] {
        let peer = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        )])));
        let desired = vec![peer.clone(); len];
        let saved = vec![peer; len];

        assert_eq!(
            pair_list_elements(
                &desired,
                &saved,
                None,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            (0..len).map(Some).collect::<Vec<_>>(),
            "list length {len} must retain stable positional pairing"
        );
    }
}

#[test]
fn list_element_pairing_marks_ties_and_zero_similarity_as_non_unique() {
    let defs = crate::schema::empty_defs_for_schema_walks();
    for len in [2, HASH_THRESHOLD] {
        let peer = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(80)),
        )])));
        let tied = pair_list_elements_with_uniqueness(
            std::slice::from_ref(&peer),
            &vec![peer.clone(); len],
            None,
            defs,
        );
        assert!(matches!(tied.as_slice(), [ListElementPairing::Ambiguous]));

        let unrelated = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        )])));
        assert_eq!(
            pair_list_elements_with_uniqueness(
                std::slice::from_ref(&peer),
                &[unrelated],
                None,
                defs,
            ),
            vec![ListElementPairing::Unmatched]
        );
    }
}

#[test]
fn list_element_pairing_marks_a_sole_positive_best_match_as_unique() {
    let map = |port| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(port)),
        )])))
    };
    let desired = [map(80)];
    let saved = [map(443), map(80)];

    assert_eq!(
        pair_list_elements_with_uniqueness(
            &desired,
            &saved,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        ),
        vec![ListElementPairing::Unique(1)]
    );
}

#[test]
fn pair_list_elements_handles_secrets_in_both_paths() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let secret = |value: &str| Value::Deferred(DeferredValue::Secret(Box::new(string(value))));
    let record = |id: i64, secret_value: &str| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("id".to_string(), Value::Concrete(ConcreteValue::Int(id))),
            ("secret".to_string(), secret(secret_value)),
        ])))
    };
    let defs = crate::schema::empty_defs_for_schema_walks();

    for len in [2, HASH_THRESHOLD] {
        let mut desired_scalars = vec![secret("same")];
        let mut saved_scalars = vec![secret("same")];
        let mut desired_maps = vec![record(0, "desired-only"), record(1, "same")];
        let mut saved_maps = vec![record(0, "saved-only"), record(1, "same")];
        for index in desired_scalars.len()..len {
            let filler = string(&format!("filler-{index}"));
            desired_scalars.push(filler.clone());
            saved_scalars.push(filler);
        }
        for index in desired_maps.len()..len {
            let filler = record(index as i64, &format!("secret-{index}"));
            desired_maps.push(filler.clone());
            saved_maps.push(filler);
        }
        let expected = (0..len).map(Some).collect::<Vec<_>>();

        assert_eq!(
            pair_list_elements(&desired_scalars, &saved_scalars, None, defs),
            expected,
            "equal secrets must pair at list length {len}"
        );
        assert_eq!(
            pair_list_elements(&desired_maps, &saved_maps, None, defs),
            expected,
            "a changed secret must pair through its matching id at list length {len}"
        );
    }
}

#[test]
fn pair_list_elements_secret_plaintext_pairing_is_threshold_independent() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let secret = |value: &str| Value::Deferred(DeferredValue::Secret(Box::new(string(value))));
    let attr_type = AttributeType::string();
    let defs = crate::schema::empty_defs_for_schema_walks();

    for len in [2, HASH_THRESHOLD] {
        let mut desired = vec![secret("p")];
        let mut saved = vec![string("p")];
        for index in 1..len {
            let filler = string(&format!("filler-{index}"));
            desired.push(filler.clone());
            saved.push(filler);
        }

        let pairs = pair_list_elements(&desired, &saved, Some(&attr_type), defs);
        let expected = (0..len).map(Some).collect::<Vec<_>>();
        assert_eq!(
            pairs, expected,
            "Secret plaintext pairing changed at list length {len}: {pairs:?}"
        );
    }
}

#[test]
fn pair_list_elements_struct_secret_field_pairing_is_threshold_independent() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let secret = |value: &str| Value::Deferred(DeferredValue::Secret(Box::new(string(value))));
    let record = |credential: Value, label: &str| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("credential".to_string(), credential),
            ("label".to_string(), string(label)),
        ])))
    };
    let attr_type = AttributeType::struct_(
        "Credential",
        vec![
            crate::schema::StructField::optional("credential", AttributeType::string()).build(),
            crate::schema::StructField::optional("label", AttributeType::string()).build(),
        ],
    );
    let defs = crate::schema::empty_defs_for_schema_walks();

    for len in [2, HASH_THRESHOLD] {
        let mut desired = vec![record(secret("p"), "desired-only")];
        let mut saved = vec![record(string("p"), "saved-only")];
        for index in 1..len {
            let filler = record(
                string(&format!("credential-{index}")),
                &format!("filler-{index}"),
            );
            desired.push(filler.clone());
            saved.push(filler);
        }

        let pairs = pair_list_elements(&desired, &saved, Some(&attr_type), defs);
        let expected = (0..len).map(Some).collect::<Vec<_>>();
        assert_eq!(
            pairs, expected,
            "Struct secret-field pairing changed at list length {len}: {pairs:?}"
        );
    }
}

#[test]
fn pair_list_elements_secret_hash_pairing_is_threshold_independent() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let secret = |value: &str| Value::Deferred(DeferredValue::Secret(Box::new(string(value))));
    let serialized = serde_json::to_string("p").expect("string serialization must succeed");
    let state_hash = RedactedSecretHash::generate(serialized.as_bytes());
    let persisted_secret = || {
        serde_json::from_value::<Value>(serde_json::json!({
            "RedactedSecret": { "hash": state_hash }
        }))
        .expect("deserialize production persisted-secret shape")
    };
    let attr_type = AttributeType::string();
    let defs = crate::schema::empty_defs_for_schema_walks();

    for len in [2, HASH_THRESHOLD] {
        let mut desired = vec![secret("p")];
        let mut saved = vec![persisted_secret()];
        for index in 1..len {
            let filler = string(&format!("filler-{index}"));
            desired.push(filler.clone());
            saved.push(filler);
        }

        let pairs = pair_list_elements(&desired, &saved, Some(&attr_type), defs);
        let expected = (0..len).map(Some).collect::<Vec<_>>();
        assert_eq!(
            pairs, expected,
            "Secret state-hash pairing changed at list length {len}: {pairs:?}"
        );
    }
}

#[test]
fn list_element_with_persisted_secret_hash_pairs_uniquely() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let desired_secret = Value::Deferred(DeferredValue::Secret(Box::new(string("p"))));
    let serialized = serde_json::to_string("p").expect("string serialization must succeed");
    let state_hash = RedactedSecretHash::generate(serialized.as_bytes());
    let persisted_secret = serde_json::from_value::<Value>(serde_json::json!({
        "RedactedSecret": { "hash": state_hash }
    }))
    .expect("deserialize production persisted-secret shape");
    let record = |credential| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "credential".to_string(),
            credential,
        )])))
    };
    let item_type = AttributeType::struct_(
        "Credential",
        vec![crate::schema::StructField::optional("credential", AttributeType::string()).build()],
    );

    assert_eq!(
        pair_list_elements_with_uniqueness(
            &[record(desired_secret)],
            &[record(persisted_secret)],
            Some(&item_type),
            crate::schema::empty_defs_for_schema_walks(),
        ),
        vec![ListElementPairing::Unique(0)]
    );
}

#[test]
fn pair_list_elements_handles_unknown_and_mixed_shapes_in_both_paths() {
    let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
    let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "id".to_string(),
        Value::Concrete(ConcreteValue::Int(7)),
    )])));
    let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
    let defs = crate::schema::empty_defs_for_schema_walks();

    for len in [3, HASH_THRESHOLD] {
        let mut desired = vec![map.clone(), string("scalar"), unknown.clone()];
        let mut saved = vec![string("scalar"), unknown.clone(), map.clone()];
        for index in desired.len()..len {
            let filler = string(&format!("filler-{index}"));
            desired.push(filler.clone());
            saved.push(filler);
        }
        let mut expected = (0..len).map(Some).collect::<Vec<_>>();
        expected[..3].copy_from_slice(&[Some(2), Some(0), None]);

        assert_eq!(
            pair_list_elements(&desired, &saved, None, defs),
            expected,
            "mixed shapes and unequal Unknowns must retain their semantics at list length {len}"
        );
    }
}

#[test]
fn hashed_shared_field_pairing_matches_fully_materialized_best_first_reference() {
    fn reference(desired: &[Value], saved: &[Value]) -> Vec<Option<usize>> {
        let defs = crate::schema::empty_defs_for_schema_walks();
        let mut candidates = Vec::new();
        for (desired_index, desired_value) in desired.iter().enumerate() {
            for (saved_index, saved_value) in saved.iter().enumerate() {
                let score = similarity_score(desired_value, saved_value, None, defs);
                if score > 0 {
                    candidates.push((score, desired_index, saved_index));
                }
            }
        }

        let mut preferred_saved = vec![None; desired.len()];
        let mut reserved = vec![false; saved.len()];
        for (desired_index, preferred_slot) in preferred_saved.iter_mut().enumerate() {
            let best_score = candidates
                .iter()
                .filter(|(_, candidate_desired, _)| *candidate_desired == desired_index)
                .map(|(score, _, _)| *score)
                .max()
                .unwrap_or(0);
            let fallback = candidates
                .iter()
                .filter(|(score, candidate_desired, _)| {
                    *candidate_desired == desired_index && *score == best_score
                })
                .map(|(_, _, saved_index)| *saved_index)
                .min();
            let preferred = candidates
                .iter()
                .filter(|(score, candidate_desired, saved_index)| {
                    *candidate_desired == desired_index
                        && *score == best_score
                        && !reserved[*saved_index]
                })
                .map(|(_, _, saved_index)| *saved_index)
                .min()
                .or(fallback);
            if let Some(saved_index) = preferred {
                *preferred_slot = Some(saved_index);
                reserved[saved_index] = true;
            }
        }

        candidates.sort_by(
            |(score_a, desired_a, saved_a), (score_b, desired_b, saved_b)| {
                let preferred_a = preferred_saved[*desired_a] == Some(*saved_a);
                let preferred_b = preferred_saved[*desired_b] == Some(*saved_b);
                score_b
                    .cmp(score_a)
                    .then_with(|| preferred_b.cmp(&preferred_a))
                    .then_with(|| desired_a.cmp(desired_b))
                    .then_with(|| saved_a.cmp(saved_b))
            },
        );

        let mut pairs = vec![None; desired.len()];
        let mut used = vec![false; saved.len()];
        for (_, desired_index, saved_index) in candidates {
            if pairs[desired_index].is_none() && !used[saved_index] {
                pairs[desired_index] = Some(saved_index);
                used[saved_index] = true;
            }
        }
        pairs
    }

    let statement = |sid: usize, action: usize, resource: usize| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "sid".to_string(),
                Value::Concrete(ConcreteValue::String(format!("Sid{sid}").into())),
            ),
            (
                "effect".to_string(),
                Value::Concrete(ConcreteValue::String("Allow".into())),
            ),
            (
                "action".to_string(),
                Value::Concrete(ConcreteValue::String(format!("Action{action}").into())),
            ),
            (
                "resource".to_string(),
                Value::Concrete(ConcreteValue::String(format!("Resource{resource}").into())),
            ),
        ])))
    };

    for variant in 0..32 {
        let desired = (0..HASH_THRESHOLD)
            .map(|index| statement(index % 5, (index + variant) % 7, (index * 3) % 11))
            .collect::<Vec<_>>();
        let saved = (0..HASH_THRESHOLD)
            .map(|index| {
                let source = (index * 7 + variant) % HASH_THRESHOLD;
                statement(
                    source % 5,
                    (source + variant / 2) % 7,
                    (source * 3 + variant / 3) % 11,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            pair_list_elements(
                &desired,
                &saved,
                None,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            reference(&desired, &saved),
            "shared-field compressed pairing drifted for variant {variant}"
        );
    }
}

#[test]
fn union_typed_string_or_list_pairing_uses_the_list_element_schema() {
    let enum_type = AttributeType::enum_(
        TypeIdentity::bare("Effect"),
        Some(vec!["Allow".to_string()]),
        vec![("Allow".to_string(), "allow".to_string())],
        None,
        None,
    );
    let attr_type = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::unordered_list(enum_type),
    ]);
    let mut desired = vec![Value::Concrete(ConcreteValue::enum_identifier("allow"))];
    let mut saved = vec![Value::Concrete(ConcreteValue::String("Allow".into()))];
    for index in 1..HASH_THRESHOLD {
        let filler = Value::Concrete(ConcreteValue::String(format!("filler-{index}").into()));
        desired.push(filler.clone());
        saved.push(filler);
    }
    let defs = crate::schema::empty_defs_for_schema_walks();

    assert_eq!(
        pair_list_elements(
            &desired,
            &saved,
            list_element_type(Some(&attr_type), defs),
            defs,
        ),
        (0..HASH_THRESHOLD).map(Some).collect::<Vec<_>>()
    );
}

#[test]
fn reference_form_pairing_is_identical_on_both_sides_of_hash_threshold() {
    use crate::provider::ReferenceScope;
    use crate::schema::{ReferenceForm, ReferenceForms, ReferenceTemplate};

    let identity = TypeIdentity::new(Some("aws"), ["lambda", "Function"], "Name");
    let attr_type = AttributeType::refined_string(Some(identity.clone()), None, None, None);
    let forms = ReferenceForms::from([(
        identity,
        ReferenceForm::Template {
            long: ReferenceTemplate::new(
                "arn:{partition}:lambda:{region}:{account}:function:{name}",
            )
            .expect("valid reference template"),
        },
    )]);
    let scope = ReferenceScope::available("aws", "us-east-1", "123456789012")
        .expect("fixture scope is valid");
    let context = crate::differ::ComparisonContext::for_test(&scope, &forms);

    for len in [HASH_THRESHOLD - 1, HASH_THRESHOLD] {
        let desired = (0..len)
            .map(|index| Value::Concrete(ConcreteValue::String(format!("function-{index}").into())))
            .collect::<Vec<_>>();
        let saved = (0..len)
            .rev()
            .map(|index| {
                Value::Concrete(ConcreteValue::String(
                    format!("arn:aws:lambda:us-east-1:123456789012:function:function-{index}")
                        .into(),
                ))
            })
            .collect::<Vec<_>>();
        let expected = (0..len)
            .map(|index| Some(len - index - 1))
            .collect::<Vec<_>>();

        assert_eq!(
            super::pair_list_elements(
                &desired,
                &saved,
                Some(&attr_type),
                crate::schema::empty_defs_for_schema_walks(),
                &context,
            ),
            expected,
            "list pairing changed at length {len}"
        );
    }
}

#[test]
fn enum_pairing_hashes_keep_dotted_strings_literal() {
    let attr_type = AttributeType::enum_(
        TypeIdentity::bare("Format"),
        Some(vec!["json".to_string()]),
        vec![],
        None,
        None,
    );
    let dotted = Value::Concrete(ConcreteValue::String("A.json".into()));
    let api = Value::Concrete(ConcreteValue::String("json".into()));
    let defs = crate::schema::empty_defs_for_schema_walks();
    let context = crate::differ::exact_comparison_context_for_tests();
    let dotted_hashes = pairing_hashes(&dotted, Some(&attr_type), defs, &context);
    let api_hashes = pairing_hashes(&api, Some(&attr_type), defs, &context);

    assert!(
        dotted_hashes.iter().all(|hash| !api_hashes.contains(hash)),
        "literal strings that compare differently must not share a pairing hash"
    );
}

#[test]
fn similarity_score_counts_shared_map_entries() {
    let first = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
        ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
    ])));
    let second = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
        ("b".to_string(), Value::Concrete(ConcreteValue::Int(3))),
    ])));
    assert_eq!(
        similarity_score(
            &first,
            &second,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        ),
        1
    );
}

#[test]
fn similarity_score_accepts_equal_scalars() {
    let scalar = Value::Concrete(ConcreteValue::Int(1));
    assert_eq!(
        similarity_score(
            &scalar,
            &scalar,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        ),
        1
    );
}

#[test]
fn merge_with_saved_malformed_list_elements_length_uses_conservative_full_merge() {
    let desired = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(80)),
        )]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        )]))),
    ]));
    let saved = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".into())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(443))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("provider-default".into())),
            ),
        ]))),
    ]));
    let malformed = ExplicitFields::ListElements {
        elements: vec![ExplicitFields::Struct {
            children: HashMap::from([
                ("port".to_string(), ExplicitFields::Leaf),
                ("description".to_string(), ExplicitFields::Leaf),
            ]),
        }],
    };

    let merged = merge_with_saved(
        &desired,
        SavedValueViews::same(&saved),
        &malformed,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );

    assert_eq!(merged, saved);
}

#[test]
fn lists_equal_large_list_correctness() {
    // Verify correctness with a list larger than HASH_THRESHOLD
    let n = 200;
    let a: Vec<Value> = (0..n)
        .map(|i| Value::Concrete(ConcreteValue::Int(i)))
        .collect();
    let b: Vec<Value> = (0..n)
        .rev()
        .map(|i| Value::Concrete(ConcreteValue::Int(i)))
        .collect();
    assert!(lists_equal(&a, &b));

    // Different content
    let mut c: Vec<Value> = (0..n)
        .map(|i| Value::Concrete(ConcreteValue::Int(i)))
        .collect();
    c[n as usize - 1] = Value::Concrete(ConcreteValue::Int(n)); // change last element
    assert!(!lists_equal(&a, &c));
}

#[test]
fn lists_equal_large_list_with_duplicates() {
    let n = 100;
    let a: Vec<Value> = (0..n)
        .flat_map(|i| {
            vec![
                Value::Concrete(ConcreteValue::Int(i)),
                Value::Concrete(ConcreteValue::Int(i)),
            ]
        })
        .collect();
    let b: Vec<Value> = (0..n)
        .rev()
        .flat_map(|i| {
            vec![
                Value::Concrete(ConcreteValue::Int(i)),
                Value::Concrete(ConcreteValue::Int(i)),
            ]
        })
        .collect();
    assert!(lists_equal(&a, &b));

    // Different multiplicities
    let mut c = a.clone();
    c[0] = Value::Concrete(ConcreteValue::Int(999));
    assert!(!lists_equal(&a, &c));
}

#[test]
fn lists_equal_large_list_of_maps() {
    // Simulates security group rules (100+ maps)
    let n = 150;
    let make_rule = |i: i64| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(i))),
            (
                "protocol".to_string(),
                Value::Concrete(ConcreteValue::String("tcp".into())),
            ),
            (
                "cidr".to_string(),
                Value::Concrete(ConcreteValue::String(format!("10.0.{}.0/24", i).into())),
            ),
        ])))
    };

    let a: Vec<Value> = (0..n).map(make_rule).collect();
    let b: Vec<Value> = (0..n).rev().map(make_rule).collect();
    assert!(lists_equal(&a, &b));
}

#[test]
fn lists_equal_performance_large_list() {
    // Benchmark: 1000-element list comparison should complete quickly
    // With O(n^2), 1000 elements = 1M comparisons; with hashing, ~1000.
    let n = 1000;
    let a: Vec<Value> = (0..n)
        .map(|i| Value::Concrete(ConcreteValue::Int(i)))
        .collect();
    let b: Vec<Value> = (0..n)
        .rev()
        .map(|i| Value::Concrete(ConcreteValue::Int(i)))
        .collect();

    let start = std::time::Instant::now();
    for _ in 0..100 {
        assert!(lists_equal(&a, &b));
    }
    let elapsed = start.elapsed();
    // Should complete well under 1 second for 100 iterations
    assert!(
        elapsed.as_secs() < 5,
        "lists_equal with 1000 elements took {:?} for 100 iterations, expected < 5s",
        elapsed
    );
}

#[test]
fn pair_list_elements_performance_large_list() {
    #[derive(Clone, Copy)]
    enum Fixture {
        AllDistinct,
        SharedEffect,
    }

    let build = |n: usize, fixture: Fixture| {
        (0..n)
            .map(|index| {
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "sid".to_string(),
                        Value::Concrete(ConcreteValue::String(format!("Statement{index}").into())),
                    ),
                    (
                        "effect".to_string(),
                        Value::Concrete(ConcreteValue::String(match fixture {
                            Fixture::AllDistinct => format!("Effect{index}").into(),
                            Fixture::SharedEffect => "Allow".into(),
                        })),
                    ),
                    (
                        "action".to_string(),
                        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                            ConcreteValue::String(format!("service:Action{index}").into()),
                        )])),
                    ),
                    (
                        "resource".to_string(),
                        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                            ConcreteValue::String(format!("arn:carina:resource/{index}").into()),
                        )])),
                    ),
                ])))
            })
            .collect::<Vec<_>>()
    };
    let measure = |n, fixture| {
        let desired = build(n, fixture);
        let mut saved = desired.clone();
        saved.reverse();
        let start = std::time::Instant::now();
        let pairs = pair_list_elements(
            &desired,
            &saved,
            None,
            crate::schema::empty_defs_for_schema_walks(),
        );
        let elapsed = start.elapsed();
        assert_eq!(pairs[0], Some(n - 1));
        assert_eq!(pairs[n - 1], Some(0));
        elapsed
    };

    for fixture in [Fixture::AllDistinct, Fixture::SharedEffect] {
        let name = match fixture {
            Fixture::AllDistinct => "all-distinct",
            Fixture::SharedEffect => "shared-effect",
        };
        for n in [100, 400, 800] {
            eprintln!("pair_list_elements {name} n={n}: {:?}", measure(n, fixture));
        }
    }

    // The known quadratic implementation takes about one second at n=800,
    // projecting to well over 30 seconds at n=5000. Two seconds leaves ample
    // headroom for slow debug CI while still reliably catching that regression.
    for (name, fixture) in [
        ("all-distinct", Fixture::AllDistinct),
        ("shared-effect", Fixture::SharedEffect),
    ] {
        let large_elapsed = measure(5_000, fixture);
        assert!(
            large_elapsed < std::time::Duration::from_secs(2),
            "pair_list_elements with 5000 {name} maps took {large_elapsed:?}, expected < 2s"
        );
    }
}

#[test]
fn merge_lists_large_list_correctness() {
    // Verify merge_lists works correctly with large lists
    let n = 50;
    let desired: Vec<Value> = (0..n)
        .map(|i| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "port".to_string(),
                Value::Concrete(ConcreteValue::Int(i)),
            )])))
        })
        .collect();
    let saved: Vec<Value> = (0..n)
        .rev()
        .map(|i| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                ("port".to_string(), Value::Concrete(ConcreteValue::Int(i))),
                (
                    "protocol".to_string(),
                    Value::Concrete(ConcreteValue::String("tcp".into())),
                ),
            ])))
        })
        .collect();

    let merged = merge_lists(
        &desired,
        &saved,
        &saved,
        &ExplicitFields::Unrecorded,
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    assert_eq!(merged.len(), n as usize);

    // Each merged element should have both port and protocol
    for item in &merged {
        if let Value::Concrete(ConcreteValue::Map(map)) = item {
            assert!(map.contains_key("port"), "Missing port in merged item");
            assert!(
                map.contains_key("protocol"),
                "Missing protocol in merged item"
            );
        } else {
            panic!("Expected Map, got {:?}", item);
        }
    }
}

#[test]
fn canonical_hash_consistency() {
    // Same value should produce same hash
    let v1 = Value::Concrete(ConcreteValue::Int(42));
    let v2 = Value::Concrete(ConcreteValue::Int(42));
    assert_eq!(v1.canonical_hash(), v2.canonical_hash());

    // Different values should (usually) produce different hashes
    let v3 = Value::Concrete(ConcreteValue::Int(43));
    assert_ne!(v1.canonical_hash(), v3.canonical_hash());

    let positive_zero = Value::Concrete(ConcreteValue::Float(0.0));
    let negative_zero = Value::Concrete(ConcreteValue::Float(-0.0));
    assert!(positive_zero.semantically_equal(&negative_zero));
    assert_eq!(
        positive_zero.canonical_hash(),
        negative_zero.canonical_hash()
    );

    // Maps with same content should hash the same regardless of insertion order
    let m1 = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
        ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
    ])));
    let m2 = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
        ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
    ])));
    assert_eq!(m1.canonical_hash(), m2.canonical_hash());
}

#[test]
fn unavailable_canonical_hash_matches_function_call_and_ignores_diagnostics() {
    let unavailable = |argument: &str, reason: &str, source_line: usize| {
        Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![Value::Concrete(ConcreteValue::String(argument.into())).into()],
            reason: UnavailableReason::from_builtin_error(reason.to_string()),
            source_line: source_line.into(),
        })
    };
    let function_call = |argument: &str| {
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![Value::Concrete(ConcreteValue::String(argument.into())).into()],
            source_line: 0.into(),
        })
    };

    assert_eq!(
        unavailable("X", "first wording", 1).canonical_hash(),
        unavailable("X", "revised wording", 99).canonical_hash(),
    );
    assert_eq!(
        unavailable("X", "diagnostic only", 7).canonical_hash(),
        function_call("X").canonical_hash(),
    );
    assert_ne!(
        unavailable("X", "same function", 7).canonical_hash(),
        function_call("Y").canonical_hash(),
    );
}

#[test]
fn resource_typed_binding_field() {
    let resource = Resource::new("s3.Bucket", "my-bucket").with_binding("my_bucket");
    assert_eq!(resource.binding, Some("my_bucket".to_string()));
    // binding should NOT be in attributes
    assert!(!resource.attributes.contains_key("_binding"));
}

#[test]
fn resource_typed_dependency_bindings_field() {
    let resource = Resource::new("ec2.Subnet", "my-subnet")
        .with_dependency_bindings(["vpc".to_string()].into_iter().collect());
    assert!(resource.dependency_bindings.contains("vpc"));
    assert_eq!(resource.dependency_bindings.len(), 1);
    // dependency_bindings should NOT be in attributes
    assert!(!resource.attributes.contains_key("_dependency_bindings"));
}

/// Set semantics: assigning the same binding twice yields exactly one
/// entry (#2228).
#[test]
fn resource_dependency_bindings_dedup_on_duplicate_insert() {
    let mut resource = Resource::new("ec2.Subnet", "my-subnet");
    resource.dependency_bindings.insert("vpc".to_string());
    resource.dependency_bindings.insert("vpc".to_string());
    assert_eq!(resource.dependency_bindings.len(), 1);
    assert!(resource.dependency_bindings.contains("vpc"));
}

/// Iteration order is deterministic (sorted) regardless of insertion
/// order (#2228).
#[test]
fn resource_dependency_bindings_iteration_is_sorted() {
    let mut resource = Resource::new("ec2.Route", "my-route");
    resource.dependency_bindings.insert("rt".to_string());
    resource
        .dependency_bindings
        .insert("tgw_attach".to_string());
    resource.dependency_bindings.insert("vpc".to_string());
    let order: Vec<&String> = resource.dependency_bindings.iter().collect();
    assert_eq!(order, vec!["rt", "tgw_attach", "vpc"]);
}

/// State-struct dependency_bindings has the same Set semantics so
/// that delete-ordering metadata is also dedup'd and stable (#2228).
#[test]
fn state_dependency_bindings_dedup_on_duplicate_insert() {
    let mut state = State::not_found(ResourceId::with_identity("ec2.Subnet", "my-subnet"));
    state.dependency_bindings.insert("vpc".to_string());
    state.dependency_bindings.insert("vpc".to_string());
    assert_eq!(state.dependency_bindings.len(), 1);
}

#[test]
fn resource_default_metadata_fields() {
    let resource = Resource::new("s3.Bucket", "my-bucket");
    assert_eq!(resource.binding, None);
    assert!(resource.dependency_bindings.is_empty());
}

#[test]
fn resource_attributes_use_value_type() {
    let resource = Resource::new("s3.Bucket", "test")
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        )
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
        );
    assert!(matches!(
        resource.get_attr("name"),
        Some(Value::Concrete(ConcreteValue::String(_)))
    ));
    assert!(matches!(
        resource.get_attr("vpc_id"),
        Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
    ));
}

#[test]
fn attrs_to_hashmap_clones_values() {
    let mut attrs = IndexMap::new();
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    );
    attrs.insert("count".to_string(), Value::Concrete(ConcreteValue::Int(5)));
    let resolved = attrs_to_hashmap(&attrs);
    assert_eq!(
        resolved.get("name"),
        Some(&Value::Concrete(ConcreteValue::String("test".into())))
    );
    assert_eq!(
        resolved.get("count"),
        Some(&Value::Concrete(ConcreteValue::Int(5)))
    );
}

#[test]
fn resource_module_source_typed_field() {
    // Real resources that belong to modules should use the typed module_source field
    // instead of storing _module/_module_instance as hidden attributes
    let module_source = ModuleSource::expanded_module(
        "web_tier",
        "web",
        ModuleInstanceScope::for_call("web_tier", Some("web"), "web"),
        CompositionCallPath::direct(CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy_in("web.crn"),
            module_name: "web_tier".to_string(),
            binding: Some("web".to_string()),
            instance: "web".to_string(),
            module_source: Some("./web".to_string()),
            module_directory: None,
            loop_origin: None,
        }),
    );
    let resource =
        Resource::new("ec2.SecurityGroup", "web_sg").with_module_source(module_source.clone());

    // Module source info should be in the typed field
    assert_eq!(resource.module_source, Some(module_source));

    // Module source info should NOT be in attributes
    assert!(!resource.attributes.contains_key("_module"));
    assert!(!resource.attributes.contains_key("_module_instance"));
}

#[test]
fn live_module_source_equality_and_hash_use_call_path_identity() {
    let source = |source_file: &str| {
        let call = CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy_in(source_file),
            module_name: "fixture".to_string(),
            binding: None,
            instance: "fixture_0000000000000000".to_string(),
            module_source: Some("./fixture".to_string()),
            module_directory: None,
            loop_origin: None,
        };
        ModuleSource::expanded_module(
            "fixture",
            "fixture_0000000000000000",
            ModuleInstanceScope::for_call("fixture", None, "fixture_0000000000000000"),
            CompositionCallPath::direct(call),
        )
    };
    let first = source("first.crn");
    let second = source("second.crn");

    assert_ne!(first, second);
    assert_eq!(
        std::collections::HashSet::from([first, second]).len(),
        2,
        "identical anonymous module display names must not collapse distinct calls",
    );
}

fn expected_stable_loop_scheduler_key(
    file: &str,
    start: usize,
    end: usize,
    kind: LoopKind,
    module_scope: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};

    fn component(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    let mut hasher = Sha256::new();
    component(&mut hasher, file.as_bytes());
    hasher.update((start as u64).to_be_bytes());
    hasher.update((end as u64).to_be_bytes());
    hasher.update([match kind {
        LoopKind::For => 0,
        LoopKind::If => 1,
    }]);
    match module_scope {
        Some(scope) => {
            hasher.update([1]);
            component(&mut hasher, scope.as_bytes());
        }
        None => hasher.update([0]),
    }
    format!("_loop_{}", hex::encode(&hasher.finalize()[..8]))
}

#[test]
fn loop_scheduler_identity_uses_the_persisted_stable_hash_contract() {
    let source = "for value in ['one'] { mock.test.Leaf { value = value } }";
    let parsed = crate::parser::parse_file(
        source,
        std::path::Path::new("stable.crn"),
        &crate::parser::ProviderContext::default(),
    )
    .expect("loop parses");
    let origin = parsed.resources[0]
        .loop_provenance
        .direct_origin()
        .expect("loop origin");
    let span = origin.source().source_span().expect("authored span");

    assert_eq!(
        origin.source().scheduler_key(),
        expected_stable_loop_scheduler_key(
            "stable.crn",
            span.start_byte(),
            span.end_byte(),
            LoopKind::For,
            None,
        ),
    );
}

#[test]
fn deserialized_unbound_loop_template_still_rejects_read_without_a_source_span() {
    let parsed = crate::parser::parse(
        "let upstream = upstream_state { source = '../upstream' }\n\
         for value in upstream.values { mock.test.Leaf { value = value } }\n",
        &crate::parser::ProviderContext::default(),
    )
    .expect("deferred loop parses");
    let crate::parser::DeferredForTarget::Unbound { template } =
        &parsed.deferred_for_expressions[0].target
    else {
        panic!("fixture must produce an unbound target")
    };
    let serialized = serde_json::to_string(template).expect("serialize loop template");
    let round_trip: UnboundLoopTemplate =
        serde_json::from_str(&serialized).expect("deserialize loop template");

    let error = round_trip.unbound_read_error(LoopKind::For);
    assert!(matches!(
        error,
        crate::parser::ParseError::UnboundLoopRead {
            kind: LoopKind::For,
            span: None,
            ..
        }
    ));
}

#[test]
fn scoped_for_and_if_loop_sources_keep_distinct_system_identities() {
    let span = SourceSpan::dummy();
    let mut for_source = LoopSource::authored(
        std::path::Path::new("module/main.crn"),
        span,
        "for value in xs {}",
        LoopKind::For,
    );
    let mut if_source = LoopSource::authored(
        std::path::Path::new("module/main.crn"),
        span,
        "if enabled {}",
        LoopKind::If,
    );
    let scope = ResourceIdentity::new("network");
    let for_parent = for_source.scheduler_key().to_string();
    let if_parent = if_source.scheduler_key().to_string();

    for_source.scope_to_module(&scope);
    if_source.scope_to_module(&scope);

    assert_eq!(
        for_source.scheduler_key(),
        expected_stable_loop_scheduler_key(
            &for_parent,
            for_source.line(),
            for_source.column(),
            LoopKind::For,
            Some("network"),
        ),
    );
    assert_eq!(
        if_source.scheduler_key(),
        expected_stable_loop_scheduler_key(
            &if_parent,
            if_source.line(),
            if_source.column(),
            LoopKind::If,
            Some("network"),
        ),
    );
}

#[test]
fn saved_nested_module_source_preserves_live_anonymous_scope() {
    let outer = ModuleInstanceScope::for_call("outer", Some("outer"), "outer");
    let child = ModuleInstanceScope::for_call("inner", Some("inner"), "inner");
    let call = CompositionCall {
        id: crate::parser::ModuleCallId::test_dummy_in("nested.crn"),
        module_name: "outer".to_string(),
        binding: Some("outer".to_string()),
        instance: "outer".to_string(),
        module_source: Some("./outer".to_string()),
        module_directory: None,
        loop_origin: None,
    };
    let live = ModuleSource::expanded_module(
        "outer",
        "outer",
        outer.with_child(&child),
        CompositionCallPath::direct(call),
    );
    let saved = SavedModuleSource::from(&live);

    assert_eq!(live.anonymous_scope(), saved.anonymous_scope());
}

#[test]
fn live_and_saved_module_sources_have_the_same_wire_shape_but_distinct_resource_types() {
    let call = CompositionCall {
        id: crate::parser::ModuleCallId::test_dummy_in("live.crn"),
        module_name: "fixture".to_string(),
        binding: Some("instance".to_string()),
        instance: "instance".to_string(),
        module_source: Some("./fixture".to_string()),
        module_directory: None,
        loop_origin: None,
    };
    let live = ModuleSource::expanded_module(
        "fixture",
        "instance",
        ModuleInstanceScope::for_call("fixture", Some("instance"), "instance"),
        CompositionCallPath::direct(call),
    );
    let resource =
        Resource::new("fixture.Resource", "instance.resource").with_module_source(live.clone());
    let encoded = serde_json::to_value(&resource).expect("serialize live resource metadata");
    assert_eq!(
        encoded["module_source"],
        serde_json::json!({
            "Module": { "name": "fixture", "instance": "instance" }
        }),
        "the live module variant must retain the historical saved-plan wire shape",
    );
    let decoded: SavedResource =
        serde_json::from_value(encoded.clone()).expect("deserialize saved resource metadata");

    assert!(matches!(
        &decoded.module_source,
        Some(SavedModuleSource::Module { name, instance, .. })
            if name == "fixture" && instance == "instance"
    ));
    assert_eq!(
        serde_json::to_value(decoded).expect("re-serialize saved resource metadata"),
        encoded,
    );

    let mut data_source = DataSource::new("fixture.Lookup", "instance.lookup");
    data_source.module_source = Some(live);
    let encoded = serde_json::to_value(&data_source).expect("serialize live data-source metadata");
    assert_eq!(
        encoded["module_source"],
        serde_json::json!({
            "Module": { "name": "fixture", "instance": "instance" }
        }),
        "data sources must use the same literal module wire variant",
    );
    let decoded: SavedDataSource =
        serde_json::from_value(encoded.clone()).expect("deserialize saved data-source metadata");
    assert!(matches!(
        &decoded.module_source,
        Some(SavedModuleSource::Module { name, instance, .. })
            if name == "fixture" && instance == "instance"
    ));
    assert_eq!(
        serde_json::to_value(decoded).expect("re-serialize saved data-source metadata"),
        encoded,
    );

    let root = Resource::new("fixture.Resource", "root").with_module_source(ModuleSource::Root);
    let encoded = serde_json::to_value(&root).expect("serialize live root metadata");
    assert_eq!(
        encoded["module_source"],
        serde_json::json!("Root"),
        "the live root variant must retain the literal saved-plan wire shape",
    );
    let decoded: SavedResource =
        serde_json::from_value(encoded.clone()).expect("deserialize saved root metadata");
    assert_eq!(decoded.module_source, Some(SavedModuleSource::Root));
}

#[test]
fn access_path_subscripts_render_with_escapes() {
    use crate::resource::{AccessPath, Subscript};

    // Integer subscript renders as `[N]`.
    let int_path = AccessPath::with_fields_and_subscripts(
        "orgs",
        "accounts",
        Vec::new(),
        vec![Subscript::Int { index: 0 }],
    );
    assert_eq!(int_path.to_dot_string(), "orgs.accounts[0]");

    // The canonical encoder can retain a double quote inside single quotes.
    let str_path = AccessPath::with_fields_and_subscripts(
        "orgs",
        "accounts",
        Vec::new(),
        vec![Subscript::Str { key: "a\"b".into() }],
    );
    assert_eq!(str_path.to_dot_string(), "orgs.accounts['a\"b']");
}

#[test]
fn string_subscript_display_uses_canonical_literal() {
    use crate::resource::{AccessPath, Subscript};

    let path = AccessPath::with_fields_and_subscripts(
        "resource",
        "tags",
        Vec::new(),
        vec![Subscript::Str { key: "Name".into() }],
    );

    assert_eq!(path.to_dot_string(), "resource.tags['Name']");
}

#[test]
fn access_path_new() {
    let path = AccessPath::new("vpc", "id");
    assert_eq!(path.binding(), "vpc");
    assert_eq!(path.attribute(), "id");
    assert!(path.leading_field_path().is_empty());
    assert_eq!(path.to_dot_string(), "vpc.id");
}

#[test]
fn access_path_with_fields() {
    let path = AccessPath::with_fields("web", "network", vec!["vpc_id".to_string()]);
    assert_eq!(path.binding(), "web");
    assert_eq!(path.attribute(), "network");
    assert_eq!(path.leading_field_path(), ["vpc_id".to_string()]);
    assert_eq!(path.to_dot_string(), "web.network.vpc_id");
}

#[test]
fn resource_ref_serde_roundtrip() {
    let value = Value::resource_ref("vpc", "id", vec![]);
    let json = serde_json::to_string(&value).unwrap();
    let deserialized: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value, deserialized);
}

#[test]
fn coalesce_serde_roundtrip_preserves_both_operands() {
    let value = Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(Value::resource_ref("up", "accounts", vec![])),
        default: Box::new(Value::Concrete(ConcreteValue::List(vec![
            Value::resource_ref("fallback", "accounts", vec![]),
        ]))),
    });
    let json = serde_json::to_string(&value).unwrap();
    let deserialized: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value, deserialized);
}

#[test]
fn resource_ref_serde_with_field_path() {
    let value = Value::resource_ref("web", "network", vec!["vpc_id".to_string()]);
    let json = serde_json::to_string(&value).unwrap();
    let deserialized: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value, deserialized);
}

#[test]
fn value_ref_helpers() {
    let value = Value::resource_ref("vpc", "vpc_id", vec!["nested".to_string()]);
    assert_eq!(value.ref_binding(), Some("vpc"));
    assert_eq!(value.ref_attribute(), Some("vpc_id"));
    assert_eq!(
        value.ref_segments(),
        Some(
            [PathSegment::Field {
                name: "nested".to_string()
            }]
            .as_slice()
        )
    );

    let non_ref = Value::Concrete(ConcreteValue::String("hello".into()));
    assert_eq!(non_ref.ref_binding(), None);
}

// Closure tests moved out: `Value::Closure` no longer exists. Closure
// construction, helper methods, and serde-skip behavior are now
// properties of `EvalValue`, exercised in `eval_value.rs`.

#[test]
fn visit_resource_refs_collects_from_all_nested_variants() {
    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::resource_ref("a", "id", vec![]),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "k".to_string(),
            Value::resource_ref("b", "id", vec![]),
        )]))),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("x".into()),
            InterpolationPart::Expr(Value::resource_ref("c", "id", vec![])),
        ])),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![Value::resource_ref("d", "id", vec![]).into()],
            source_line: (0).into(),
        }),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::resource_ref(
            "e",
            "id",
            vec![],
        )))),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("f", "id", vec![])),
            default: Box::new(Value::resource_ref("g", "id", vec![])),
        }),
        Value::Concrete(ConcreteValue::String("plain".into())),
    ]));

    let mut collected: Vec<String> = Vec::new();
    value.visit_resource_refs(&mut |path| {
        collected.push(path.binding().to_string());
    });
    collected.sort();
    assert_eq!(collected, vec!["a", "b", "c", "d", "e", "f", "g"]);
}

#[test]
fn resource_ref_usage_marks_only_the_coalesce_value_optional() {
    let value = Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(Value::resource_ref("optional", "id", vec![])),
        default: Box::new(Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("nested_optional", "id", vec![])),
            default: Box::new(Value::resource_ref("required", "id", vec![])),
        })),
    });
    let mut collected = Vec::new();
    value.visit_resource_refs_with_usage(&mut |path, usage| {
        collected.push((path.binding().to_string(), usage));
    });

    assert_eq!(
        collected,
        vec![
            ("optional".to_string(), ResourceRefUsage::CoalesceValue),
            (
                "nested_optional".to_string(),
                ResourceRefUsage::CoalesceValue,
            ),
            ("required".to_string(), ResourceRefUsage::Required),
        ],
    );
}

#[test]
fn visit_binding_refs_collects_from_all_nested_variants() {
    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::Deferred(DeferredValue::BindingRef {
            binding: "a".to_string(),
        }),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "k".to_string(),
            Value::Deferred(DeferredValue::BindingRef {
                binding: "b".to_string(),
            }),
        )]))),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("x".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::BindingRef {
                binding: "c".to_string(),
            })),
        ])),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![
                Value::Deferred(DeferredValue::BindingRef {
                    binding: "d".to_string(),
                })
                .into(),
            ],
            source_line: (0).into(),
        }),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::BindingRef {
                binding: "e".to_string(),
            },
        )))),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::Deferred(DeferredValue::BindingRef {
                binding: "f".to_string(),
            })),
            default: Box::new(Value::Deferred(DeferredValue::BindingRef {
                binding: "g".to_string(),
            })),
        }),
    ]));

    let mut collected = Vec::new();
    value.visit_binding_refs(&mut |binding| {
        collected.push(binding.to_string());
    });
    collected.sort();

    assert_eq!(collected, vec!["a", "b", "c", "d", "e", "f", "g"]);
}

#[test]
fn visit_resource_refs_on_leaf_variants_calls_nothing() {
    for v in [
        Value::Concrete(ConcreteValue::String("s".into())),
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Float(1.0)),
        Value::Concrete(ConcreteValue::Bool(true)),
    ] {
        let mut count = 0;
        v.visit_resource_refs(&mut |_| count += 1);
        assert_eq!(count, 0);
    }
}

// canonicalize() — see #2227

#[test]
fn canonicalize_leaves_simple_values_alone() {
    for v in [
        Value::Concrete(ConcreteValue::String("s".into())),
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Float(1.5)),
        Value::Concrete(ConcreteValue::Bool(true)),
        Value::resource_ref("vpc", "id", vec![]),
    ] {
        assert_eq!(v.clone().canonicalize(), v);
    }
}

#[test]
fn canonicalize_collapses_all_literal_interpolation_to_string() {
    let v = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("foo".into()),
        InterpolationPart::Literal("bar".into()),
    ]));
    assert_eq!(
        v.canonicalize(),
        Value::Concrete(ConcreteValue::String("foobar".into()))
    );
}

#[test]
fn canonicalize_collapses_single_literal_interpolation_to_string() {
    let v = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("foo".into()),
    ]));
    assert_eq!(
        v.canonicalize(),
        Value::Concrete(ConcreteValue::String("foo".into()))
    );
}

#[test]
fn canonicalize_unwraps_single_scalar_expr_interpolation() {
    // Every string-shaped scalar (String / Int / Float / Bool) is
    // folded into a flat `Value::Concrete(ConcreteValue::String)` when wrapped in a
    // single-element interpolation.
    let cases: Vec<(Value, &str)> = vec![
        (Value::Concrete(ConcreteValue::String("foo".into())), "foo"),
        (Value::Concrete(ConcreteValue::Int(42)), "42"),
        (Value::Concrete(ConcreteValue::Float(1.5)), "1.5"),
        (Value::Concrete(ConcreteValue::Bool(true)), "true"),
    ];
    for (inner, expected) in cases {
        let label = format!("{:?}", inner);
        let v = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
            inner,
        )]));
        assert_eq!(
            v.canonicalize(),
            Value::Concrete(ConcreteValue::String(expected.into())),
            "case {} failed",
            label
        );
    }
}

#[test]
fn canonicalize_merges_adjacent_literals() {
    let v = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("a".into()),
        InterpolationPart::Literal("b".into()),
        InterpolationPart::Expr(Value::resource_ref("vpc", "id", vec![])),
        InterpolationPart::Literal("c".into()),
        InterpolationPart::Literal("d".into()),
    ]));
    assert_eq!(
        v.canonicalize(),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("ab".into()),
            InterpolationPart::Expr(Value::resource_ref("vpc", "id", vec![])),
            InterpolationPart::Literal("cd".into()),
        ]))
    );
}

#[test]
fn canonicalize_folds_simple_expr_into_literal_then_merges() {
    // ["prefix-", Expr(String("foo")), "-suffix"] -> "prefix-foo-suffix"
    let v = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::Concrete(ConcreteValue::String("foo".into()))),
        InterpolationPart::Literal("-suffix".into()),
    ]));
    assert_eq!(
        v.canonicalize(),
        Value::Concrete(ConcreteValue::String("prefix-foo-suffix".into()))
    );
}

#[test]
fn canonicalize_keeps_resource_ref_expr_in_interpolation() {
    let parts = vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::resource_ref("vpc", "id", vec![])),
    ];
    let v = Value::Deferred(DeferredValue::Interpolation(parts.clone()));
    assert_eq!(
        v.canonicalize(),
        Value::Deferred(DeferredValue::Interpolation(parts))
    );
}

#[test]
fn canonicalize_recurses_into_list_and_map() {
    let v = Value::Concrete(ConcreteValue::List(vec![
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("foo".into()),
        ])),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "k".to_string(),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("bar".into()),
            ])),
        )]))),
    ]));
    assert_eq!(
        v.canonicalize(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("foo".into())),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "k".to_string(),
                Value::Concrete(ConcreteValue::String("bar".into())),
            )]))),
        ]))
    );
}

#[test]
fn canonicalize_recurses_into_function_call_args() {
    let v = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("foo".into()),
            ]))
            .into(),
        ],
        source_line: (0).into(),
    });
    assert_eq!(
        v.canonicalize(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![Value::Concrete(ConcreteValue::String("foo".into())).into()],
            source_line: (0).into(),
        })
    );
}

#[test]
fn canonicalize_recurses_into_secret() {
    let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
        DeferredValue::Interpolation(vec![InterpolationPart::Literal("hidden".into())]),
    ))));
    assert_eq!(
        v.canonicalize(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("hidden".into())
        ))))
    );
}

#[test]
fn canonicalize_collapses_nested_interpolation() {
    // Inner `Interpolation([Literal("foo")])` collapses to `String("foo")`,
    // its outer `Expr(String("foo"))` folds into a Literal, and the
    // single-literal Interpolation collapses to `String("foo")`.
    let v = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("foo".into()),
        ])),
    )]));
    assert_eq!(
        v.canonicalize(),
        Value::Concrete(ConcreteValue::String("foo".into()))
    );
}

#[test]
fn canonicalize_resolves_secret_interpolation_to_a_secret_string() {
    // A `Secret(_)` inside an interpolation participates in ordinary string
    // evaluation, but the completed string must retain the Secret wrapper.
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("password".into()),
    ))));
    let v = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("db-".into()),
        InterpolationPart::Expr(secret.clone()),
    ]));
    assert_eq!(
        v.canonicalize(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("db-password".into()),
        )))),
    );

    // A sole Secret expression is normalized the same way, without nesting
    // a second Secret wrapper.
    let only_secret = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        secret.clone(),
    )]));
    assert_eq!(only_secret.canonicalize(), secret,);
}

#[test]
fn canonicalize_is_idempotent() {
    let inputs = vec![
        // All-Literal Interpolation collapses to String on first call.
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("foo".into()),
            InterpolationPart::Literal("bar".into()),
        ])),
        // ResourceRef-bearing Interpolation stays as Interpolation.
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("a".into()),
            InterpolationPart::Expr(Value::resource_ref("x", "y", vec![])),
            InterpolationPart::Literal("b".into()),
        ])),
        // Nested Interpolation inside an Expr.
        Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("nested".into()),
            ])),
        )])),
        // Secret-wrapped Interpolation: canonicalize recurses through
        // Secret at the Value level but keeps Secret(Expr) wrapped.
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::Interpolation(vec![InterpolationPart::Literal("hidden".into())]),
        )))),
        // List, Map, FunctionCall — recursion shapes.
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("x".into()),
        )])),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "k".to_string(),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("v".into()),
            ])),
        )]))),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![
                Value::Deferred(DeferredValue::Interpolation(vec![
                    InterpolationPart::Literal("x".into()),
                ]))
                .into(),
            ],
            source_line: (0).into(),
        }),
    ];
    for v in inputs {
        let once = v.clone().canonicalize();
        let twice = once.clone().canonicalize();
        assert_eq!(once, twice, "canonicalize must be idempotent for {:?}", v);
    }
}

// ----- RFC #2371 stage 2 (#2377): Value::Deferred(DeferredValue::Unknown) semantics -----

#[test]
fn unknown_never_equals_unknown() {
    // Two Value::Deferred(DeferredValue::Unknown) must compare unequal even when their reasons
    // are identical — an unknown value is, by definition, not equal to
    // anything (including itself). Without this, the differ would
    // suppress real diffs between two attributes that both happen to
    // be "unresolved upstream" of the same binding.
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let a = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path: path.clone(),
    }));
    let b = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path,
    }));
    assert_ne!(
        a, b,
        "two `Value::Deferred(DeferredValue::Unknown)`s must never compare equal"
    );
}

#[test]
fn unknown_never_equals_concrete() {
    let a = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
    let b = Value::Concrete(ConcreteValue::String("anything".into()));
    assert_ne!(a, b);
    assert_ne!(b, a);
}

#[test]
fn unknown_for_variants_never_equal() {
    assert_ne!(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey))
    );
    assert_ne!(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue))
    );
}

#[test]
fn unknown_hash_is_deterministic_per_path() {
    use std::hash::Hasher;
    fn hash_of(v: &Value) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash_into(&mut h);
        h.finish()
    }
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let a = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path: path.clone(),
    }));
    let b = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path,
    }));
    assert_eq!(
        hash_of(&a),
        hash_of(&b),
        "same UpstreamExport path must hash to the same value"
    );
}

#[test]
fn unknown_hash_differs_per_path() {
    use std::hash::Hasher;
    fn hash_of(v: &Value) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash_into(&mut h);
        h.finish()
    }
    let p1 = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let p2 = AccessPath::with_fields("network", "vpc", vec!["cidr_block".into()]);
    let a = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path: p1,
    }));
    let b = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path: p2,
    }));
    assert_ne!(hash_of(&a), hash_of(&b));
}

#[test]
fn unknown_hash_does_not_panic_for_for_variants() {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)).hash_into(&mut h);
    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)).hash_into(&mut h);
    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)).hash_into(&mut h);
    let _ = h.finish();
}

#[test]
fn unknown_round_trips_through_saved_plan_serde() {
    // Residual deferred-for templates in saved plans need the typed reason in
    // order to substitute their loop key/value at apply time. Runtime
    // provider/state conversion still rejects Unknown through value_to_json.
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let value = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path: path.clone(),
    }));
    let json = serde_json::to_string(&value).expect("serialize saved-plan unknown");
    let decoded: Value = serde_json::from_str(&json).expect("deserialize saved-plan unknown");
    assert!(matches!(
        decoded,
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: decoded_path,
        })) if decoded_path == path
    ));
}

#[test]
fn deserializing_reference_rooted_access_canonicalizes_the_reference() {
    let encoded = serde_json::json!({
        "Access": {
            "base": { "BindingRef": { "binding": "roles[0]" } },
            "step": { "kind": "field", "name": "identifier" }
        }
    });

    let decoded: Value = serde_json::from_value(encoded).expect("valid deferred access encoding");
    assert_eq!(
        decoded,
        Value::resource_ref("roles[0]", "identifier", vec![]),
        "deserialization must enter through the same canonical constructor"
    );
}

#[test]
fn upstream_unknown_wire_tags_remain_stable() {
    let path = AccessPath::new("network", "vpc_id");
    let export = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
        path: path.clone(),
    }));
    let export_json =
        r#"{"Unknown":{"UpstreamRef":{"path":{"binding":"network","attribute":"vpc_id"}}}}"#;
    assert_eq!(serde_json::to_string(&export).unwrap(), export_json);
    match serde_json::from_str::<Value>(export_json).unwrap() {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: decoded_path,
        })) => assert_eq!(decoded_path, path),
        other => panic!("expected UpstreamExport, got {other:?}"),
    }

    let all_exports = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamAllExports {
        binding: "network".into(),
    }));
    let all_exports_json = r#"{"Unknown":{"UpstreamBareRef":{"binding":"network"}}}"#;
    assert_eq!(
        serde_json::to_string(&all_exports).unwrap(),
        all_exports_json
    );
    match serde_json::from_str::<Value>(all_exports_json).unwrap() {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamAllExports { binding })) => {
            assert_eq!(binding, "network")
        }
        other => panic!("expected UpstreamAllExports, got {other:?}"),
    }
}

#[test]
fn human_display_separates_type_from_name_with_space() {
    let id = ResourceId::with_provider_identity(
        "awscc",
        "iam.OidcProvider",
        "bs.bootstrap.oidc_provider",
        None,
    );
    assert_eq!(
        format!("{}", id.human()),
        "awscc.iam.OidcProvider bs.bootstrap.oidc_provider",
        "human format must put a single space between provider.resource_type and name, \
         so the type/address boundary is visible (carina-rs/carina#2572)"
    );
}

#[test]
fn human_display_omits_provider_segment_when_empty() {
    // ResourceId::with_identity (no provider) is used in some early-pipeline code paths
    // and in tests. The human format must still separate type from name.
    let id = ResourceId::with_identity("s3.Bucket", "state_bucket");
    assert_eq!(format!("{}", id.human()), "s3.Bucket state_bucket");
}

#[test]
fn human_display_does_not_alter_logical_display() {
    // The default Display remains the canonical dotted form because state
    // files, hashmap keys, binding fallbacks, and DSL identifiers all rely
    // on it. Only the human() wrapper changes shape.
    let id = ResourceId::with_provider_identity("aws", "s3.Bucket", "state_bucket", None);
    assert_eq!(format!("{}", id), "aws.s3.Bucket.state_bucket");
}

#[test]
fn human_display_handles_absent_identity() {
    let id = ResourceId::pending_with_provider("awscc", "ec2.Vpc", None);
    assert_eq!(format!("{}", id), "awscc.ec2.Vpc.<pending>");
    assert_eq!(format!("{}", id.human()), "awscc.ec2.Vpc <pending>");

    let local_id = ResourceId::pending("custom.Type");
    assert_eq!(format!("{}", local_id), "custom.Type.<pending>");
    assert_eq!(format!("{}", local_id.human()), "custom.Type <pending>");
}

// ---------------------------------------------------------------------
// Phase 1 of RFC #2972 — `Value::as_concrete` / `as_deferred`
// borrowing projections. The full type-split happens later; for now we
// only verify the accessors disambiguate the two axes correctly.
// ---------------------------------------------------------------------

#[test]
fn as_concrete_returns_projection_for_concrete_variants() {
    // Sample one variant per concrete arm; the match in `as_concrete`
    // is exhaustive so this is enough to catch a future variant added
    // to the wrong axis.
    let cases: Vec<(Value, &'static str)> = vec![
        (Value::Concrete(ConcreteValue::String("x".into())), "String"),
        (Value::Concrete(ConcreteValue::Int(1)), "Int"),
        (Value::Concrete(ConcreteValue::Float(1.5)), "Float"),
        (Value::Concrete(ConcreteValue::Bool(true)), "Bool"),
        (
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60))),
            "Duration",
        ),
        (Value::Concrete(ConcreteValue::List(vec![])), "List"),
        (
            Value::Concrete(ConcreteValue::StringList(vec![])),
            "StringList",
        ),
        (Value::Concrete(ConcreteValue::Map(IndexMap::new())), "Map"),
    ];
    for (v, label) in cases {
        assert!(
            v.as_concrete().is_some(),
            "{label} must project to ConcreteValueRef",
        );
        assert!(
            v.as_deferred().is_none(),
            "{label} must NOT project to DeferredValueRef",
        );
    }
}

#[test]
fn as_deferred_returns_projection_for_deferred_variants() {
    let cases: Vec<(Value, &'static str)> = vec![
        (
            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
            "ResourceRef",
        ),
        (
            Value::Deferred(DeferredValue::BindingRef {
                binding: "bootstrap".to_string(),
            }),
            "BindingRef",
        ),
        (
            Value::Deferred(DeferredValue::Interpolation(vec![])),
            "Interpolation",
        ),
        (
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::join,
                args: vec![],
                source_line: (0).into(),
            }),
            "FunctionCall",
        ),
        (
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("inner".into()),
            )))),
            "Secret",
        ),
        (
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)),
            "Unknown",
        ),
    ];
    for (v, label) in cases {
        assert!(
            v.as_deferred().is_some(),
            "{label} must project to DeferredValueRef",
        );
        assert!(
            v.as_concrete().is_none(),
            "{label} must NOT project to ConcreteValueRef",
        );
    }
}

#[test]
fn as_concrete_borrows_inner_string_without_clone() {
    // The accessor must hand out a &str pointing into the original
    // value, not a copy. Easy proof: pointer equality on the bytes.
    let v = Value::Concrete(ConcreteValue::String("hello".into()));
    let Some(ConcreteValueRef::String(borrowed)) = v.as_concrete() else {
        panic!("expected String projection");
    };
    let Value::Concrete(ConcreteValue::String(ref original)) = v else {
        unreachable!("v constructed as Value::Concrete(ConcreteValue::String)");
    };
    assert_eq!(
        borrowed.expose_plaintext().as_ptr(),
        original.expose_plaintext().as_ptr()
    );
}

#[test]
fn as_concrete_list_borrows_underlying_slice() {
    let v = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Int(1)),
        Value::Concrete(ConcreteValue::Int(2)),
    ]));
    let Some(ConcreteValueRef::List(items)) = v.as_concrete() else {
        panic!("expected List projection");
    };
    let Value::Concrete(ConcreteValue::List(ref original)) = v else {
        unreachable!("v constructed as Value::Concrete(ConcreteValue::List)");
    };
    assert_eq!(items.as_ptr(), original.as_ptr());
    assert_eq!(items.len(), 2);
}

#[test]
fn directives_provider_instance_round_trips_serde() {
    let d = Directives {
        provider_instance: Some("us".to_string()),
        ..Directives::default()
    };
    let json = serde_json::to_string(&d).unwrap();
    let back: Directives = serde_json::from_str(&json).unwrap();
    assert_eq!(back.provider_instance, d.provider_instance);
}

#[test]
fn directives_serialises_without_provider_instance_when_none() {
    let d = Directives::default();
    let json = serde_json::to_string(&d).unwrap();
    assert!(
        !json.contains("provider_instance"),
        "None provider_instance should be skipped in serialisation, got {}",
        json
    );
}

#[test]
fn directives_provider_instance_deserialises_from_legacy_json_without_field() {
    let legacy =
        r#"{ "force_delete": false, "create_before_destroy": false, "prevent_destroy": false }"#;
    let d: Directives = serde_json::from_str(legacy).unwrap();
    assert!(d.provider_instance.is_none());
}

// carina#3136: navigate_value_path — the single path-walking primitive
// loop-variable field access resolves through.
mod navigate_value_path_tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::Concrete(ConcreteValue::String(v.to_string().into()))
    }
    fn map(pairs: &[(&str, Value)]) -> Value {
        let mut m = indexmap::IndexMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Concrete(ConcreteValue::Map(m))
    }

    #[test]
    fn one_level_field() {
        // root = { k = "v" }, path = _.k  → "v"
        let root = map(&[("k", s("v"))]);
        let path = AccessPath::new("o", "k");
        assert_eq!(navigate_value_path(&root, &path), Some(s("v")));
    }

    #[test]
    fn two_level_field_the_real_registry_shape() {
        // root = { resource_record = { name = "n1" } }
        // path = _.resource_record.name  → "n1"
        let root = map(&[("resource_record", map(&[("name", s("n1"))]))]);
        let path = AccessPath::with_fields("opt", "resource_record", vec!["name".into()]);
        assert_eq!(navigate_value_path(&root, &path), Some(s("n1")));
    }

    #[test]
    fn list_int_subscript_after_field() {
        // root = { vals = ["a", "b"] }, path = _.vals[1]  → "b"
        let root = map(&[(
            "vals",
            Value::Concrete(ConcreteValue::List(vec![s("a"), s("b")])),
        )]);
        let path = AccessPath::with_fields_and_subscripts(
            "o",
            "vals",
            vec![],
            vec![Subscript::Int { index: 1 }],
        );
        assert_eq!(navigate_value_path(&root, &path), Some(s("b")));
    }

    #[test]
    fn map_str_subscript() {
        // root = { tags = { Name = "x" } }, path = _.tags["Name"]  → "x"
        let root = map(&[("tags", map(&[("Name", s("x"))]))]);
        let path = AccessPath::with_fields_and_subscripts(
            "o",
            "tags",
            vec![],
            vec![Subscript::Str { key: "Name".into() }],
        );
        assert_eq!(navigate_value_path(&root, &path), Some(s("x")));
    }

    #[test]
    fn missing_field_is_none() {
        let root = map(&[("k", s("v"))]);
        let path = AccessPath::new("o", "absent");
        assert_eq!(navigate_value_path(&root, &path), None);
    }

    #[test]
    fn scalar_root_rejects_field_access() {
        // root is a scalar string — `.k` is meaningless → None (the
        // Site B "not a resource" guard's correct domain).
        let root = s("scalar");
        let path = AccessPath::new("o", "k");
        assert_eq!(navigate_value_path(&root, &path), None);
    }

    #[test]
    fn out_of_range_index_is_none() {
        let root = map(&[("vals", Value::Concrete(ConcreteValue::List(vec![s("a")])))]);
        let path = AccessPath::with_fields_and_subscripts(
            "o",
            "vals",
            vec![],
            vec![Subscript::Int { index: 5 }],
        );
        assert_eq!(navigate_value_path(&root, &path), None);
    }

    #[test]
    fn deferred_hop_is_not_reported_as_a_concrete_navigation_result() {
        let root = map(&[(
            "nested",
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        )]);
        let path = AccessPath::with_fields("o", "nested", vec!["name".into()]);

        assert_eq!(navigate_value_path(&root, &path), None);
    }

    #[test]
    fn secret_leaf_keeps_its_tag() {
        // root = { tok = secret("s") }; navigating to it must re-wrap
        // the Secret so the tag survives end-to-end (#2439 parity).
        let root = map(&[(
            "tok",
            Value::Deferred(DeferredValue::Secret(Box::new(s("s")))),
        )]);
        let path = AccessPath::new("o", "tok");
        assert_eq!(
            navigate_value_path(&root, &path),
            Some(Value::Deferred(DeferredValue::Secret(Box::new(s("s")))))
        );
    }

    #[test]
    fn field_through_secret_wrapped_map() {
        // root = secret({ name = "n" }); _.name peels the secret to
        // descend, then re-wraps the leaf.
        let inner = map(&[("name", s("n"))]);
        let root = map(&[(
            "rr",
            Value::Deferred(DeferredValue::Secret(Box::new(inner))),
        )]);
        let path = AccessPath::with_fields("o", "rr", vec!["name".into()]);
        assert_eq!(
            navigate_value_path(&root, &path),
            Some(Value::Deferred(DeferredValue::Secret(Box::new(s("n")))))
        );
    }
}
