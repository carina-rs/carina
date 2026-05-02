use super::*;

#[test]
fn value_serde_round_trip() {
    let values = vec![
        Value::String("hello".to_string()),
        Value::Int(42),
        Value::Float(2.5),
        Value::Float(-0.5),
        Value::Bool(true),
        Value::List(vec![Value::String("a".to_string()), Value::Int(1)]),
        Value::Map(IndexMap::from([
            ("key".to_string(), Value::String("val".to_string())),
            ("num".to_string(), Value::Int(10)),
        ])),
        Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
        Value::String("dedicated".to_string()),
        Value::String("InstanceTenancy.dedicated".to_string()),
        Value::Interpolation(vec![
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "vpc".to_string(),
                "id".to_string(),
                vec![],
            )),
            InterpolationPart::Literal("-suffix".to_string()),
        ]),
        Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                ]),
            ],
        },
        Value::Secret(Box::new(Value::String("my-password".to_string()))),
    ];

    for value in values {
        let json = serde_json::to_string(&value).unwrap();
        let deserialized: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value, deserialized, "Round-trip failed for {:?}", value);
    }
}

#[test]
fn resource_id_serde_round_trip() {
    let id = ResourceId::with_provider("awscc", "ec2.Vpc", "main-vpc");
    let json = serde_json::to_string(&id).unwrap();
    let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, deserialized);
}

// The "anonymous, awaiting `name` extraction" state is type-distinct
// from a bound name, so the parser cannot accidentally produce a
// `ResourceId` whose `name` is the empty string and have it be
// mistaken for a valid identifier (#2225).

#[test]
fn resource_name_pending_is_distinct_from_bound_empty() {
    let pending = ResourceName::Pending;
    let bound_empty = ResourceName::Bound(String::new());
    assert_ne!(pending, bound_empty);
    assert!(pending.is_pending());
    assert!(!bound_empty.is_pending());
}

#[test]
fn resource_id_pending_serde_round_trips_as_empty_string() {
    // V5 state files persist `name` as a plain JSON string. To preserve
    // backward compatibility, ResourceName::Pending serializes to "" and
    // deserializes from "" — round-trip is exact.
    let id = ResourceId {
        provider: "aws".to_string(),
        resource_type: "ec2.Subnet".to_string(),
        name: ResourceName::Pending,
    };
    let json = serde_json::to_string(&id).unwrap();
    assert!(json.contains("\"name\":\"\""), "got: {json}");
    let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, deserialized);
    assert!(deserialized.name.is_pending());
}

#[test]
fn resource_id_bound_serde_round_trips_as_string() {
    let id = ResourceId::with_provider("aws", "ec2.Subnet", "my-subnet");
    let json = serde_json::to_string(&id).unwrap();
    assert!(json.contains("\"name\":\"my-subnet\""), "got: {json}");
    let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, deserialized);
    match deserialized.name {
        ResourceName::Bound(s) => assert_eq!(s, "my-subnet"),
        _ => panic!("expected Bound"),
    }
}

/// The AC test from #2225: lookups keyed by `ResourceId` must remain
/// valid across the name-resolution pass. This is achieved by ensuring
/// that the parser starts with `Pending`, then any rename to `Bound`
/// produces a stable identifier.
/// We assert that two different mutation paths produce equal IDs.
#[test]
fn resource_id_rename_pending_to_bound() {
    let mut id = ResourceId {
        provider: "aws".to_string(),
        resource_type: "ec2.Subnet".to_string(),
        name: ResourceName::Pending,
    };
    // The post-pass converts Pending → Bound with the extracted name.
    id.set_name("app-subnet".to_string());
    match &id.name {
        ResourceName::Bound(s) => assert_eq!(s, "app-subnet"),
        _ => panic!("expected Bound after set_name"),
    }
    // After renaming, the same string can produce an equal ResourceId
    // from any other code path (e.g. building a key for a sibling map).
    let constructed = ResourceId::with_provider("aws", "ec2.Subnet", "app-subnet");
    assert_eq!(id, constructed);
}

#[test]
fn state_serde_round_trip() {
    let mut attrs = HashMap::new();
    attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
    attrs.insert("versioning".to_string(), Value::Bool(true));

    let state = State::existing(
        ResourceId::with_provider("aws", "s3.Bucket", "my-bucket"),
        attrs,
    )
    .with_identifier("my-bucket");

    let json = serde_json::to_string(&state).unwrap();
    let deserialized: State = serde_json::from_str(&json).unwrap();
    assert_eq!(state, deserialized);
}

#[test]
fn lifecycle_config_serde_with_create_before_destroy() {
    let config = LifecycleConfig {
        create_before_destroy: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: LifecycleConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(config, deserialized);
    assert!(deserialized.create_before_destroy);
}

#[test]
fn lifecycle_config_backward_compatible_deserialize() {
    // Old JSON without all fields should deserialize with defaults
    let json = r#"{"create_before_destroy":true}"#;
    let config: LifecycleConfig = serde_json::from_str(json).unwrap();
    assert!(config.create_before_destroy);
    assert!(!config.force_delete);
    assert!(!config.prevent_destroy);
}

#[test]
fn lifecycle_config_with_force_delete() {
    let config = LifecycleConfig {
        force_delete: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: LifecycleConfig = serde_json::from_str(&json).unwrap();
    assert!(deserialized.force_delete);
    assert!(!deserialized.create_before_destroy);
    assert!(!deserialized.prevent_destroy);
}

#[test]
fn semantically_equal_lists_same_order() {
    let a = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_different_order() {
    let a = Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(2)]);
    let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_different_content() {
    let a = Value::List(vec![Value::Int(1), Value::Int(2)]);
    let b = Value::List(vec![Value::Int(1), Value::Int(3)]);
    assert!(!a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_different_lengths() {
    let a = Value::List(vec![Value::Int(1), Value::Int(2)]);
    let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert!(!a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_with_duplicates() {
    let a = Value::List(vec![Value::Int(1), Value::Int(1), Value::Int(2)]);
    let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(1)]);
    assert!(a.semantically_equal(&b));

    // Different multiplicities should not be equal
    let c = Value::List(vec![Value::Int(1), Value::Int(1), Value::Int(2)]);
    let d = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(2)]);
    assert!(!c.semantically_equal(&d));
}

#[test]
fn semantically_equal_empty_lists() {
    let a = Value::List(vec![]);
    let b = Value::List(vec![]);
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_of_maps_different_order() {
    let mut map1 = IndexMap::new();
    map1.insert("port".to_string(), Value::Int(80));
    map1.insert("protocol".to_string(), Value::String("tcp".to_string()));

    let mut map2 = IndexMap::new();
    map2.insert("port".to_string(), Value::Int(443));
    map2.insert("protocol".to_string(), Value::String("tcp".to_string()));

    let a = Value::List(vec![Value::Map(map1.clone()), Value::Map(map2.clone())]);
    let b = Value::List(vec![Value::Map(map2), Value::Map(map1)]);
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_lists_of_strings() {
    let a = Value::List(vec![
        Value::String("b".to_string()),
        Value::String("a".to_string()),
    ]);
    let b = Value::List(vec![
        Value::String("a".to_string()),
        Value::String("b".to_string()),
    ]);
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_non_list_values() {
    // Non-list values should use regular equality
    assert!(Value::Int(42).semantically_equal(&Value::Int(42)));
    assert!(!Value::Int(42).semantically_equal(&Value::Int(43)));
    assert!(
        Value::String("hello".to_string()).semantically_equal(&Value::String("hello".to_string()))
    );
    assert!(Value::Bool(true).semantically_equal(&Value::Bool(true)));
}

#[test]
fn semantically_equal_nested_lists() {
    // Lists inside maps are compared order-insensitively via recursive semantically_equal
    let mut map1 = IndexMap::new();
    map1.insert(
        "ports".to_string(),
        Value::List(vec![Value::Int(80), Value::Int(443)]),
    );

    let mut map2 = IndexMap::new();
    map2.insert(
        "ports".to_string(),
        Value::List(vec![Value::Int(443), Value::Int(80)]),
    );

    let a = Value::Map(map1);
    let b = Value::Map(map2);
    assert!(a.semantically_equal(&b));
}

#[test]
fn semantically_equal_maps_different_keys() {
    let mut map1 = IndexMap::new();
    map1.insert("a".to_string(), Value::Int(1));

    let mut map2 = IndexMap::new();
    map2.insert("b".to_string(), Value::Int(1));

    assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
}

#[test]
fn semantically_equal_maps_different_sizes() {
    let mut map1 = IndexMap::new();
    map1.insert("a".to_string(), Value::Int(1));

    let mut map2 = IndexMap::new();
    map2.insert("a".to_string(), Value::Int(1));
    map2.insert("b".to_string(), Value::Int(2));

    assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
}

#[test]
fn merge_with_saved_map_fills_extra_keys() {
    let desired = Value::Map(IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::String("ip-name".to_string()),
        ),
        ("a_record".to_string(), Value::Bool(true)),
    ]));
    let saved = Value::Map(IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::String("ip-name".to_string()),
        ),
        ("a_record".to_string(), Value::Bool(true)),
        ("aaaa_record".to_string(), Value::Bool(false)),
    ]));

    let merged = merge_with_saved(&desired, &saved);
    let expected = Value::Map(IndexMap::from([
        (
            "hostname_type".to_string(),
            Value::String("ip-name".to_string()),
        ),
        ("a_record".to_string(), Value::Bool(true)),
        ("aaaa_record".to_string(), Value::Bool(false)),
    ]));
    assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
}

#[test]
fn merge_with_saved_desired_wins() {
    let desired = Value::Map(IndexMap::from([("a".to_string(), Value::Int(10))]));
    let saved = Value::Map(IndexMap::from([
        ("a".to_string(), Value::Int(5)),
        ("b".to_string(), Value::Int(20)),
    ]));

    let merged = merge_with_saved(&desired, &saved);
    let expected = Value::Map(IndexMap::from([
        ("a".to_string(), Value::Int(10)),
        ("b".to_string(), Value::Int(20)),
    ]));
    assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
}

#[test]
fn merge_with_saved_list_of_maps() {
    let desired = Value::List(vec![Value::Map(IndexMap::from([(
        "port".to_string(),
        Value::Int(80),
    )]))]);
    let saved = Value::List(vec![Value::Map(IndexMap::from([
        ("port".to_string(), Value::Int(80)),
        ("protocol".to_string(), Value::String("tcp".to_string())),
    ]))]);

    let merged = merge_with_saved(&desired, &saved);
    let expected = Value::List(vec![Value::Map(IndexMap::from([
        ("port".to_string(), Value::Int(80)),
        ("protocol".to_string(), Value::String("tcp".to_string())),
    ]))]);
    assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
}

#[test]
fn merge_with_saved_non_map() {
    let desired = Value::String("hello".to_string());
    let saved = Value::String("world".to_string());
    let merged = merge_with_saved(&desired, &saved);
    assert_eq!(merged, Value::String("hello".to_string()));

    let desired = Value::Int(42);
    let saved = Value::Int(99);
    let merged = merge_with_saved(&desired, &saved);
    assert_eq!(merged, Value::Int(42));
}

#[test]
fn lists_equal_large_list_correctness() {
    // Verify correctness with a list larger than HASH_THRESHOLD
    let n = 200;
    let a: Vec<Value> = (0..n).map(Value::Int).collect();
    let b: Vec<Value> = (0..n).rev().map(Value::Int).collect();
    assert!(lists_equal(&a, &b));

    // Different content
    let mut c: Vec<Value> = (0..n).map(Value::Int).collect();
    c[n as usize - 1] = Value::Int(n); // change last element
    assert!(!lists_equal(&a, &c));
}

#[test]
fn lists_equal_large_list_with_duplicates() {
    let n = 100;
    let a: Vec<Value> = (0..n)
        .flat_map(|i| vec![Value::Int(i), Value::Int(i)])
        .collect();
    let b: Vec<Value> = (0..n)
        .rev()
        .flat_map(|i| vec![Value::Int(i), Value::Int(i)])
        .collect();
    assert!(lists_equal(&a, &b));

    // Different multiplicities
    let mut c = a.clone();
    c[0] = Value::Int(999);
    assert!(!lists_equal(&a, &c));
}

#[test]
fn lists_equal_large_list_of_maps() {
    // Simulates security group rules (100+ maps)
    let n = 150;
    let make_rule = |i: i64| {
        Value::Map(IndexMap::from([
            ("port".to_string(), Value::Int(i)),
            ("protocol".to_string(), Value::String("tcp".to_string())),
            (
                "cidr".to_string(),
                Value::String(format!("10.0.{}.0/24", i)),
            ),
        ]))
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
    let a: Vec<Value> = (0..n).map(Value::Int).collect();
    let b: Vec<Value> = (0..n).rev().map(Value::Int).collect();

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
fn merge_lists_large_list_correctness() {
    // Verify merge_lists works correctly with large lists
    let n = 50;
    let desired: Vec<Value> = (0..n)
        .map(|i| Value::Map(IndexMap::from([("port".to_string(), Value::Int(i))])))
        .collect();
    let saved: Vec<Value> = (0..n)
        .rev()
        .map(|i| {
            Value::Map(IndexMap::from([
                ("port".to_string(), Value::Int(i)),
                ("protocol".to_string(), Value::String("tcp".to_string())),
            ]))
        })
        .collect();

    let merged = merge_lists(&desired, &saved);
    assert_eq!(merged.len(), n as usize);

    // Each merged element should have both port and protocol
    for item in &merged {
        if let Value::Map(map) = item {
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
    let v1 = Value::Int(42);
    let v2 = Value::Int(42);
    assert_eq!(v1.canonical_hash(), v2.canonical_hash());

    // Different values should (usually) produce different hashes
    let v3 = Value::Int(43);
    assert_ne!(v1.canonical_hash(), v3.canonical_hash());

    // Maps with same content should hash the same regardless of insertion order
    let m1 = Value::Map(IndexMap::from([
        ("a".to_string(), Value::Int(1)),
        ("b".to_string(), Value::Int(2)),
    ]));
    let m2 = Value::Map(IndexMap::from([
        ("b".to_string(), Value::Int(2)),
        ("a".to_string(), Value::Int(1)),
    ]));
    assert_eq!(m1.canonical_hash(), m2.canonical_hash());
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
    let mut state = State::not_found(ResourceId::new("ec2.Subnet", "my-subnet"));
    state.dependency_bindings.insert("vpc".to_string());
    state.dependency_bindings.insert("vpc".to_string());
    assert_eq!(state.dependency_bindings.len(), 1);
}

#[test]
fn resource_typed_virtual_field() {
    let resource = Resource::new("_virtual", "web").with_kind(ResourceKind::Virtual {
        module_name: "web_tier".to_string(),
        instance: "web".to_string(),
    });
    assert!(resource.is_virtual());
    // _virtual should NOT be in attributes
    assert!(!resource.attributes.contains_key("_virtual"));
}

#[test]
fn resource_default_metadata_fields() {
    let resource = Resource::new("s3.Bucket", "my-bucket");
    assert_eq!(resource.binding, None);
    assert!(resource.dependency_bindings.is_empty());
    assert!(!resource.is_virtual());
}

#[test]
fn resource_kind_enum_managed_by_default() {
    let resource = Resource::new("s3.Bucket", "my-bucket");
    assert_eq!(resource.kind, ResourceKind::Managed);
    assert!(!resource.is_virtual());
    assert!(!resource.is_data_source());
}

#[test]
fn resource_kind_enum_virtual_carries_module_info() {
    let resource = Resource::new("_virtual", "web").with_kind(ResourceKind::Virtual {
        module_name: "web_tier".to_string(),
        instance: "web".to_string(),
    });
    assert!(resource.is_virtual());
    assert!(!resource.is_data_source());
    // Module info is in the kind, not in attributes
    assert!(!resource.attributes.contains_key("_module"));
    assert!(!resource.attributes.contains_key("_module_instance"));
    // Can extract module info from the kind
    match &resource.kind {
        ResourceKind::Virtual {
            module_name,
            instance,
        } => {
            assert_eq!(module_name, "web_tier");
            assert_eq!(instance, "web");
        }
        _ => panic!("Expected Virtual kind"),
    }
}

#[test]
fn resource_kind_enum_data_source() {
    let resource = Resource::new("s3.Bucket", "my-bucket").with_kind(ResourceKind::DataSource);
    assert!(resource.is_data_source());
    assert!(!resource.is_virtual());
}

#[test]
fn resource_attributes_use_value_type() {
    let resource = Resource::new("s3.Bucket", "test")
        .with_attribute("name", Value::String("my-bucket".to_string()))
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
        );
    assert!(matches!(resource.get_attr("name"), Some(Value::String(_))));
    assert!(matches!(
        resource.get_attr("vpc_id"),
        Some(Value::ResourceRef { .. })
    ));
}

#[test]
fn attrs_to_hashmap_clones_values() {
    let mut attrs = IndexMap::new();
    attrs.insert("name".to_string(), Value::String("test".to_string()));
    attrs.insert("count".to_string(), Value::Int(5));
    let resolved = attrs_to_hashmap(&attrs);
    assert_eq!(
        resolved.get("name"),
        Some(&Value::String("test".to_string()))
    );
    assert_eq!(resolved.get("count"), Some(&Value::Int(5)));
}

#[test]
fn resource_module_source_typed_field() {
    // Real resources that belong to modules should use the typed module_source field
    // instead of storing _module/_module_instance as hidden attributes
    let resource =
        Resource::new("ec2.SecurityGroup", "web_sg").with_module_source(ModuleSource::Module {
            name: "web_tier".to_string(),
            instance: "web".to_string(),
        });

    // Module source info should be in the typed field
    assert_eq!(
        resource.module_source,
        Some(ModuleSource::Module {
            name: "web_tier".to_string(),
            instance: "web".to_string(),
        })
    );

    // Module source info should NOT be in attributes
    assert!(!resource.attributes.contains_key("_module"));
    assert!(!resource.attributes.contains_key("_module_instance"));
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

    // String subscript with embedded quote escapes through {:?}.
    let str_path = AccessPath::with_fields_and_subscripts(
        "orgs",
        "accounts",
        Vec::new(),
        vec![Subscript::Str {
            key: "a\"b".to_string(),
        }],
    );
    let rendered = str_path.to_dot_string();
    assert!(
        rendered.contains("\\\""),
        "embedded quote must escape, got: {rendered}"
    );
}

#[test]
fn access_path_new() {
    let path = AccessPath::new("vpc", "id");
    assert_eq!(path.binding(), "vpc");
    assert_eq!(path.attribute(), "id");
    assert!(path.field_path().is_empty());
    assert_eq!(path.to_dot_string(), "vpc.id");
}

#[test]
fn access_path_with_fields() {
    let path = AccessPath::with_fields("web", "network", vec!["vpc_id".to_string()]);
    assert_eq!(path.binding(), "web");
    assert_eq!(path.attribute(), "network");
    assert_eq!(path.field_path(), ["vpc_id".to_string()]);
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
        value.ref_field_path(),
        Some(["nested".to_string()].as_slice())
    );

    let non_ref = Value::String("hello".to_string());
    assert_eq!(non_ref.ref_binding(), None);
}

// Closure tests moved out: `Value::Closure` no longer exists. Closure
// construction, helper methods, and serde-skip behavior are now
// properties of `EvalValue`, exercised in `eval_value.rs`.

#[test]
fn visit_refs_collects_from_all_nested_variants() {
    let value = Value::List(vec![
        Value::resource_ref("a", "id", vec![]),
        Value::Map(IndexMap::from([(
            "k".to_string(),
            Value::resource_ref("b", "id", vec![]),
        )])),
        Value::Interpolation(vec![
            InterpolationPart::Literal("x".to_string()),
            InterpolationPart::Expr(Value::resource_ref("c", "id", vec![])),
        ]),
        Value::FunctionCall {
            name: "join".to_string(),
            args: vec![Value::resource_ref("d", "id", vec![])],
        },
        Value::Secret(Box::new(Value::resource_ref("e", "id", vec![]))),
        Value::String("plain".to_string()),
    ]);

    let mut collected: Vec<String> = Vec::new();
    value.visit_refs(&mut |path| {
        collected.push(path.binding().to_string());
    });
    collected.sort();
    assert_eq!(collected, vec!["a", "b", "c", "d", "e"]);
}

#[test]
fn visit_refs_on_leaf_variants_calls_nothing() {
    for v in [
        Value::String("s".into()),
        Value::Int(1),
        Value::Float(1.0),
        Value::Bool(true),
    ] {
        let mut count = 0;
        v.visit_refs(&mut |_| count += 1);
        assert_eq!(count, 0);
    }
}

// canonicalize() — see #2227

#[test]
fn canonicalize_leaves_simple_values_alone() {
    for v in [
        Value::String("s".into()),
        Value::Int(1),
        Value::Float(1.5),
        Value::Bool(true),
        Value::resource_ref("vpc", "id", vec![]),
    ] {
        assert_eq!(v.clone().canonicalize(), v);
    }
}

#[test]
fn canonicalize_collapses_all_literal_interpolation_to_string() {
    let v = Value::Interpolation(vec![
        InterpolationPart::Literal("foo".into()),
        InterpolationPart::Literal("bar".into()),
    ]);
    assert_eq!(v.canonicalize(), Value::String("foobar".into()));
}

#[test]
fn canonicalize_collapses_single_literal_interpolation_to_string() {
    let v = Value::Interpolation(vec![InterpolationPart::Literal("foo".into())]);
    assert_eq!(v.canonicalize(), Value::String("foo".into()));
}

#[test]
fn canonicalize_unwraps_single_scalar_expr_interpolation() {
    // Every string-shaped scalar (String / Int / Float / Bool) is
    // folded into a flat `Value::String` when wrapped in a
    // single-element interpolation.
    let cases: Vec<(Value, &str)> = vec![
        (Value::String("foo".into()), "foo"),
        (Value::Int(42), "42"),
        (Value::Float(1.5), "1.5"),
        (Value::Bool(true), "true"),
    ];
    for (inner, expected) in cases {
        let label = format!("{:?}", inner);
        let v = Value::Interpolation(vec![InterpolationPart::Expr(inner)]);
        assert_eq!(
            v.canonicalize(),
            Value::String(expected.into()),
            "case {} failed",
            label
        );
    }
}

#[test]
fn canonicalize_merges_adjacent_literals() {
    let v = Value::Interpolation(vec![
        InterpolationPart::Literal("a".into()),
        InterpolationPart::Literal("b".into()),
        InterpolationPart::Expr(Value::resource_ref("vpc", "id", vec![])),
        InterpolationPart::Literal("c".into()),
        InterpolationPart::Literal("d".into()),
    ]);
    assert_eq!(
        v.canonicalize(),
        Value::Interpolation(vec![
            InterpolationPart::Literal("ab".into()),
            InterpolationPart::Expr(Value::resource_ref("vpc", "id", vec![])),
            InterpolationPart::Literal("cd".into()),
        ])
    );
}

#[test]
fn canonicalize_folds_simple_expr_into_literal_then_merges() {
    // ["prefix-", Expr(String("foo")), "-suffix"] -> "prefix-foo-suffix"
    let v = Value::Interpolation(vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::String("foo".into())),
        InterpolationPart::Literal("-suffix".into()),
    ]);
    assert_eq!(v.canonicalize(), Value::String("prefix-foo-suffix".into()));
}

#[test]
fn canonicalize_keeps_resource_ref_expr_in_interpolation() {
    let parts = vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::resource_ref("vpc", "id", vec![])),
    ];
    let v = Value::Interpolation(parts.clone());
    assert_eq!(v.canonicalize(), Value::Interpolation(parts));
}

#[test]
fn canonicalize_recurses_into_list_and_map() {
    let v = Value::List(vec![
        Value::Interpolation(vec![InterpolationPart::Literal("foo".into())]),
        Value::Map(IndexMap::from([(
            "k".to_string(),
            Value::Interpolation(vec![InterpolationPart::Literal("bar".into())]),
        )])),
    ]);
    assert_eq!(
        v.canonicalize(),
        Value::List(vec![
            Value::String("foo".into()),
            Value::Map(IndexMap::from([(
                "k".to_string(),
                Value::String("bar".into()),
            )])),
        ])
    );
}

#[test]
fn canonicalize_recurses_into_function_call_args() {
    let v = Value::FunctionCall {
        name: "upper".into(),
        args: vec![Value::Interpolation(vec![InterpolationPart::Literal(
            "foo".into(),
        )])],
    };
    assert_eq!(
        v.canonicalize(),
        Value::FunctionCall {
            name: "upper".into(),
            args: vec![Value::String("foo".into())],
        }
    );
}

#[test]
fn canonicalize_recurses_into_secret() {
    let v = Value::Secret(Box::new(Value::Interpolation(vec![
        InterpolationPart::Literal("hidden".into()),
    ])));
    assert_eq!(
        v.canonicalize(),
        Value::Secret(Box::new(Value::String("hidden".into())))
    );
}

#[test]
fn canonicalize_collapses_nested_interpolation() {
    // Inner `Interpolation([Literal("foo")])` collapses to `String("foo")`,
    // its outer `Expr(String("foo"))` folds into a Literal, and the
    // single-literal Interpolation collapses to `String("foo")`.
    let v = Value::Interpolation(vec![InterpolationPart::Expr(Value::Interpolation(vec![
        InterpolationPart::Literal("foo".into()),
    ]))]);
    assert_eq!(v.canonicalize(), Value::String("foo".into()));
}

#[test]
fn canonicalize_keeps_secret_expr_in_interpolation() {
    // A `Secret(_)` inside an `Expr` must NOT be folded into a Literal:
    // doing so would let the secret travel as plain text and bypass
    // redaction in plan display, state serialization, and logging.
    let secret = Value::Secret(Box::new(Value::String("password".into())));
    let parts = vec![
        InterpolationPart::Literal("db-".into()),
        InterpolationPart::Expr(secret.clone()),
    ];
    let v = Value::Interpolation(parts.clone());
    assert_eq!(v.canonicalize(), Value::Interpolation(parts));

    // Even when the Secret is the only Expr part, it must stay wrapped.
    let only_secret = Value::Interpolation(vec![InterpolationPart::Expr(secret.clone())]);
    assert_eq!(
        only_secret.canonicalize(),
        Value::Interpolation(vec![InterpolationPart::Expr(secret)]),
    );
}

#[test]
fn canonicalize_is_idempotent() {
    let inputs = vec![
        // All-Literal Interpolation collapses to String on first call.
        Value::Interpolation(vec![
            InterpolationPart::Literal("foo".into()),
            InterpolationPart::Literal("bar".into()),
        ]),
        // ResourceRef-bearing Interpolation stays as Interpolation.
        Value::Interpolation(vec![
            InterpolationPart::Literal("a".into()),
            InterpolationPart::Expr(Value::resource_ref("x", "y", vec![])),
            InterpolationPart::Literal("b".into()),
        ]),
        // Nested Interpolation inside an Expr.
        Value::Interpolation(vec![InterpolationPart::Expr(Value::Interpolation(vec![
            InterpolationPart::Literal("nested".into()),
        ]))]),
        // Secret-wrapped Interpolation: canonicalize recurses through
        // Secret at the Value level but keeps Secret(Expr) wrapped.
        Value::Secret(Box::new(Value::Interpolation(vec![
            InterpolationPart::Literal("hidden".into()),
        ]))),
        // List, Map, FunctionCall — recursion shapes.
        Value::List(vec![Value::String("x".into())]),
        Value::Map(IndexMap::from([(
            "k".to_string(),
            Value::Interpolation(vec![InterpolationPart::Literal("v".into())]),
        )])),
        Value::FunctionCall {
            name: "upper".into(),
            args: vec![Value::Interpolation(vec![InterpolationPart::Literal(
                "x".into(),
            )])],
        },
    ];
    for v in inputs {
        let once = v.clone().canonicalize();
        let twice = once.clone().canonicalize();
        assert_eq!(once, twice, "canonicalize must be idempotent for {:?}", v);
    }
}
