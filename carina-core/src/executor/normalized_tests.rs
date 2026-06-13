use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::executor::normalized::{
    apply_desired_normalization, apply_desired_normalization_slice, restore_stripped_attributes,
    run_desired_normalization_stages, strip_attributes_matching,
    strip_provider_boundary_attributes,
};
use crate::parser::ProviderConfig;
use crate::provider::{
    BoxFuture, NoopNormalizer, ProviderFactory, ProviderNormalizer, ProviderResult, ready_noop,
};
use crate::resource::{
    AccessPath, ConcreteValue, DeferredValue, InterpolationPart, Resource, ResourceId, State,
    UnknownReason, Value, contains_resource_ref,
};
use crate::schema::SchemaRegistry;

#[derive(Clone)]
struct RecordingNormalizer {
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingNormalizer {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl ProviderNormalizer for RecordingNormalizer {
    fn normalize_desired<'a>(&'a self, _resources: &'a mut [Resource]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push("normalize_desired".to_string());
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
    ) -> BoxFuture<'a, ()> {
        ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> BoxFuture<'a, ()> {
        ready_noop()
    }

    fn merge_default_tags<'a>(
        &'a self,
        _resources: &'a mut [Resource],
        _default_tags: &'a IndexMap<String, Value>,
        _registry: &'a SchemaRegistry,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push("merge_default_tags".to_string());
        })
    }
}

fn provider_config(default_tags: IndexMap<String, Value>) -> ProviderConfig {
    ProviderConfig {
        name: "test".to_string(),
        attributes: IndexMap::new(),
        default_tags,
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    }
}

fn string_value(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string()))
}

struct AliasFactory;

impl ProviderFactory for AliasFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "Test"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, crate::schema::AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        String::new()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn crate::provider::Provider>>> {
        unreachable!("test factory does not create providers")
    }

    fn schemas(&self) -> Vec<crate::schema::ResourceSchema> {
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, enum_identity};

        vec![ResourceSchema::new("thing").attribute(AttributeSchema::new(
            "mode",
            AttributeType::enum_(
                enum_identity("Mode", Some("test.thing")),
                Some(vec!["friendly".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        ))]
    }

    fn get_enum_alias_reverse(
        &self,
        _resource_type: &str,
        attr_name: &str,
        value: &str,
    ) -> Option<String> {
        (attr_name == "mode" && value == "friendly").then(|| "canonical".to_string())
    }
}

fn order_schema() -> SchemaRegistry {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("thing")
            .attribute(AttributeSchema::new(
                "subjects",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            ))
            .attribute(AttributeSchema::new(
                "ref_subjects",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )),
    );
    schemas
}

#[tokio::test]
async fn desired_normalization_runs_stages_in_order() {
    struct OrderNormalizer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderNormalizer for OrderNormalizer {
        fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource]) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let subjects = resources[0].get_attr("subjects").cloned();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("normalize_desired:{subjects:?}"));
                resources[0].set_attr("mode", string_value("Mode.friendly"));
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }

        fn merge_default_tags<'a>(
            &'a self,
            resources: &'a mut [Resource],
            _default_tags: &'a IndexMap<String, Value>,
            _registry: &'a SchemaRegistry,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let mode = resources[0].get_attr("mode").cloned();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("merge_default_tags:{mode:?}"));
            })
        }
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let normalizer = OrderNormalizer {
        calls: Arc::clone(&calls),
    };
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr("subjects", string_value("one"));
    let mut default_tags = IndexMap::new();
    default_tags.insert("ManagedBy".to_string(), string_value("carina"));
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];
    let schemas = order_schema();

    let normalized = apply_desired_normalization(
        resource,
        &[provider_config(default_tags)],
        &normalizer,
        &factories,
        &schemas,
    )
    .await;

    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![
            format!(
                "normalize_desired:{:?}",
                Some(Value::Concrete(ConcreteValue::StringList(vec![
                    "one".to_string()
                ])))
            ),
            format!(
                "merge_default_tags:{:?}",
                Some(string_value("Mode.friendly"))
            ),
        ],
        "canonicalize must run before normalize_desired and normalize_desired must run before merge_default_tags"
    );
    assert_eq!(
        normalized.as_resource().get_attr("mode"),
        Some(&string_value("canonical")),
        "resolve_enum_aliases must run after merge_default_tags"
    );
}

#[tokio::test]
async fn merge_default_tags_runs_only_for_non_empty_provider_default_tags() {
    let normalizer = RecordingNormalizer::new();
    let resource = Resource::new("test", "thing");
    let mut default_tags = IndexMap::new();
    default_tags.insert("ManagedBy".to_string(), string_value("carina"));

    let _normalized = apply_desired_normalization(
        resource,
        &[
            provider_config(IndexMap::new()),
            provider_config(default_tags),
        ],
        &normalizer,
        &[],
        &SchemaRegistry::new(),
    )
    .await;

    assert_eq!(
        normalizer
            .calls()
            .iter()
            .filter(|call| call.as_str() == "merge_default_tags")
            .count(),
        1,
        "merge_default_tags must be invoked exactly once for the one non-empty provider config"
    );
}

#[tokio::test]
async fn in_place_desired_normalization_does_not_canonicalize_resources() {
    let normalizer = RecordingNormalizer::new();
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr("subjects", string_value("one"));
    let mut resources = vec![resource];
    let schemas = order_schema();

    run_desired_normalization_stages(&mut resources, &[], &normalizer, &[], &schemas).await;

    assert_eq!(
        resources[0].get_attr("subjects"),
        Some(&string_value("one")),
        "in-place desired normalization must leave canonicalize to the caller so plan code can run it before strip/restore"
    );
}

#[tokio::test]
async fn apply_desired_normalization_strips_and_restores_deferred_resource_refs() {
    struct RefRejectingNormalizer;

    impl ProviderNormalizer for RefRejectingNormalizer {
        fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource]) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                assert!(
                    resources[0].get_attr("role_arn").is_none(),
                    "deferred resource refs must be stripped before provider normalization"
                );
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }

        fn merge_default_tags<'a>(
            &'a self,
            _resources: &'a mut [Resource],
            _default_tags: &'a IndexMap<String, Value>,
            _registry: &'a SchemaRegistry,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }
    }

    let deferred = Value::resource_ref("role", "arn", vec![]);
    let mut resource = Resource::new("test", "thing");
    resource.set_attr("role_arn", deferred.clone());

    let normalized = apply_desired_normalization(
        resource,
        &[],
        &RefRejectingNormalizer,
        &[],
        &SchemaRegistry::new(),
    )
    .await;

    assert_eq!(
        normalized.as_resource().get_attr("role_arn"),
        Some(&deferred),
        "the stripped deferred resource ref must be restored after normalization"
    );
}

#[tokio::test]
async fn desired_normalization_slice_canonicalizes_strips_stages_and_restores_refs() {
    struct RefRejectingNormalizer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderNormalizer for RefRejectingNormalizer {
        fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource]) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                assert!(
                    resources[0].get_attr("ref_subjects").is_none(),
                    "ref-bearing attributes must be stripped before normalize_desired"
                );
                self.calls
                    .lock()
                    .unwrap()
                    .push("normalize_desired".to_string());
                resources[0].set_attr("mode", string_value("Mode.friendly"));
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ()> {
            ready_noop()
        }

        fn merge_default_tags<'a>(
            &'a self,
            resources: &'a mut [Resource],
            _default_tags: &'a IndexMap<String, Value>,
            _registry: &'a SchemaRegistry,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                assert!(
                    resources[0].get_attr("ref_subjects").is_none(),
                    "ref-bearing attributes must stay stripped through merge_default_tags"
                );
                self.calls
                    .lock()
                    .unwrap()
                    .push("merge_default_tags".to_string());
            })
        }
    }

    let ref_interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("role:".to_string()),
        InterpolationPart::Expr(Value::resource_ref("role", "arn", vec![])),
    ]));
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr("subjects", string_value("one"));
    resource.set_attr("ref_subjects", ref_interpolation.clone());

    let calls = Arc::new(Mutex::new(Vec::new()));
    let normalizer = RefRejectingNormalizer {
        calls: Arc::clone(&calls),
    };
    let mut default_tags = IndexMap::new();
    default_tags.insert("ManagedBy".to_string(), string_value("carina"));
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];
    let schemas = order_schema();
    let mut resources = vec![resource];

    apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &normalizer,
        &factories,
        &schemas,
    )
    .await;

    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![
            "normalize_desired".to_string(),
            "merge_default_tags".to_string()
        ]
    );
    assert_eq!(
        resources[0].get_attr("subjects"),
        Some(&Value::Concrete(ConcreteValue::StringList(vec![
            "one".to_string()
        ]))),
        "slice wrapper must canonicalize before running provider stages"
    );
    assert_eq!(
        resources[0].get_attr("ref_subjects"),
        Some(&ref_interpolation),
        "ref-bearing interpolation must round-trip unchanged"
    );
    assert_eq!(
        resources[0].get_attr("mode"),
        Some(&string_value("canonical")),
        "slice wrapper must run enum alias resolution after provider stages"
    );
}

// ----- RFC #2371 stage 2 (#2377): strip-and-restore round trip -----

#[test]
fn strip_and_restore_unknown_attributes_round_trip() {
    let mut r = Resource::new("test.t", "n");
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    r.attributes.insert(
        "group_description".into(),
        Value::Concrete(ConcreteValue::String("web".into())),
    );
    r.attributes.insert(
        "vpc_id".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        })),
    );
    let mut tags: IndexMap<String, Value> = IndexMap::new();
    tags.insert(
        "Name".into(),
        Value::Concrete(ConcreteValue::String("web-sg".into())),
    );
    r.attributes
        .insert("tags".into(), Value::Concrete(ConcreteValue::Map(tags)));
    r.attributes.insert(
        "nested_unknown".into(),
        Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
            DeferredValue::Unknown(UnknownReason::UpstreamRef { path }),
        )])),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = strip_provider_boundary_attributes(&mut resources);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["group_description".to_string(), "tags".to_string()]
    );
    assert_eq!(stripped.len(), 1);
    let entries = stripped.values().next().unwrap();
    assert_eq!(entries.len(), 2);

    restore_stripped_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "restore must put attributes back at their original index"
    );
    assert!(matches!(
        resources[0].get_attr("vpc_id"),
        Some(Value::Deferred(DeferredValue::Unknown(_)))
    ));
    match resources[0].get_attr("nested_unknown") {
        Some(Value::Concrete(ConcreteValue::List(items))) => {
            assert!(matches!(
                items[0],
                Value::Deferred(DeferredValue::Unknown(_))
            ));
        }
        other => panic!(
            "nested_unknown should still be Value::Concrete(ConcreteValue::List), got {:?}",
            other
        ),
    }
}

#[test]
fn strip_provider_boundary_attributes_recurses_into_unknown_wrappers() {
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let unknown = || {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        }))
    };

    let mut resource = Resource::new("test.t", "n");
    resource.attributes.insert("direct".into(), unknown());
    resource.attributes.insert(
        "list".into(),
        Value::Concrete(ConcreteValue::List(vec![unknown()])),
    );
    resource.attributes.insert(
        "map".into(),
        Value::Concrete(ConcreteValue::Map({
            let mut m = IndexMap::new();
            m.insert("k".into(), unknown());
            m
        })),
    );
    resource.attributes.insert(
        "interpolation".into(),
        Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
            unknown(),
        )])),
    );
    resource.attributes.insert(
        "function".into(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: "f".into(),
            args: vec![unknown()],
        }),
    );
    resource.attributes.insert(
        "secret".into(),
        Value::Deferred(DeferredValue::Secret(Box::new(unknown()))),
    );
    resource.attributes.insert(
        "plain_string".into(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    resource
        .attributes
        .insert("plain_int".into(), Value::Concrete(ConcreteValue::Int(1)));
    let mut resources = vec![resource];

    let stripped = strip_provider_boundary_attributes(&mut resources);

    assert_eq!(stripped.values().next().unwrap().len(), 6);
    assert_eq!(
        resources[0].attributes.keys().cloned().collect::<Vec<_>>(),
        vec!["plain_string".to_string(), "plain_int".to_string()]
    );
}

#[test]
fn restore_unknown_attributes_after_normalize_injection() {
    let mut r = Resource::new("test.t", "n");
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    r.attributes.insert(
        "a".into(),
        Value::Concrete(ConcreteValue::String("a-val".into())),
    );
    r.attributes.insert(
        "b".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        })),
    );
    r.attributes.insert(
        "c".into(),
        Value::Concrete(ConcreteValue::String("c-val".into())),
    );
    let mut resources = vec![r];

    let stripped = strip_provider_boundary_attributes(&mut resources);
    resources[0].attributes.insert(
        "z".into(),
        Value::Concrete(ConcreteValue::String("z-val".into())),
    );
    restore_stripped_attributes(&mut resources, stripped);

    let order: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order,
        vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "z".to_string()
        ],
        "originals must keep their indices; injected attrs trail them"
    );
}

#[test]
fn strip_and_restore_for_expression_unknowns_round_trip() {
    let mut r = Resource::new("test.t", "n");
    r.attributes.insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("static".into())),
    );
    r.attributes.insert(
        "target_id".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
    );
    r.attributes.insert(
        "items".into(),
        Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
            DeferredValue::Unknown(UnknownReason::ForKey),
        )])),
    );
    r.attributes.insert(
        "index".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = strip_provider_boundary_attributes(&mut resources);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["name".to_string()],
        "all three for-expression Unknown attributes must be stripped"
    );
    assert_eq!(stripped.values().next().unwrap().len(), 3);

    restore_stripped_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "for-expression Unknowns must be restored at their original indices"
    );
    assert!(matches!(
        resources[0].get_attr("target_id"),
        Some(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ForValue
        )))
    ));
    assert!(matches!(
        resources[0].get_attr("index"),
        Some(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ForIndex
        )))
    ));
}

#[test]
fn strip_provider_boundary_attributes_covers_unknown_for_variants() {
    let mut resource = Resource::new("test.t", "n");
    resource.attributes.insert(
        "for_value".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
    );
    resource.attributes.insert(
        "for_key".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)),
    );
    resource.attributes.insert(
        "for_index".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
    );
    resource.attributes.insert(
        "nested_for_value".into(),
        Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
            DeferredValue::Unknown(UnknownReason::ForValue),
        )])),
    );
    let mut resources = vec![resource];

    let stripped = strip_provider_boundary_attributes(&mut resources);

    assert_eq!(stripped.values().next().unwrap().len(), 4);
    assert!(resources[0].attributes.is_empty());
}

// ----- #2387: strip-and-restore round trip for ResourceRef -----

#[test]
fn strip_and_restore_resource_ref_round_trip() {
    let mut r = Resource::new("test.t", "n");
    let path = AccessPath::with_fields("admins", "group_id", vec![]);
    r.attributes.insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("static".into())),
    );
    r.attributes.insert(
        "group_id".into(),
        Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
    );
    let mut nested_map: IndexMap<String, Value> = IndexMap::new();
    nested_map.insert(
        "ref".into(),
        Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
    );
    r.attributes.insert(
        "policy".into(),
        Value::Concrete(ConcreteValue::Map(nested_map)),
    );
    r.attributes.insert(
        "label".into(),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: path.clone(),
            })),
        ])),
    );
    r.attributes.insert(
        "joined".into(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: "join".into(),
            args: vec![
                Value::Concrete(ConcreteValue::String(",".into())),
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
            ],
        }),
    );
    r.attributes.insert(
        "secret_ref".into(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::ResourceRef { path: path.clone() },
        )))),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = strip_attributes_matching(&mut resources, &contains_resource_ref);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["name".to_string()],
        "every attribute that recursively contains a ResourceRef must be stripped"
    );
    let entries = stripped.values().next().expect("one resource stripped");
    assert_eq!(entries.len(), 5);

    restore_stripped_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "ResourceRef-bearing attributes must be restored at their original indices"
    );
    assert!(matches!(
        resources[0].get_attr("group_id"),
        Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
    ));
    assert!(matches!(
        resources[0].get_attr("label"),
        Some(Value::Deferred(DeferredValue::Interpolation(_)))
    ));
    assert!(matches!(
        resources[0].get_attr("joined"),
        Some(Value::Deferred(DeferredValue::FunctionCall { .. }))
    ));
    assert!(matches!(
        resources[0].get_attr("secret_ref"),
        Some(Value::Deferred(DeferredValue::Secret(_)))
    ));
}

#[test]
fn strip_unified_predicate_covers_unknown_and_ref() {
    let mut r = Resource::new("test.t", "n");
    let path = AccessPath::with_fields("admins", "group_id", vec![]);
    r.attributes.insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("static".into())),
    );
    r.attributes.insert(
        "vpc_id".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]),
        })),
    );
    r.attributes.insert(
        "group_id".into(),
        Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
    );
    let mut resources = vec![r];

    let stripped = strip_provider_boundary_attributes(&mut resources);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(after_strip, vec!["name".to_string()]);
    assert_eq!(stripped.values().next().unwrap().len(), 2);

    restore_stripped_attributes(&mut resources, stripped);
    assert!(matches!(
        resources[0].get_attr("vpc_id"),
        Some(Value::Deferred(DeferredValue::Unknown(_)))
    ));
    assert!(matches!(
        resources[0].get_attr("group_id"),
        Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
    ));
}

#[tokio::test]
async fn apply_desired_normalization_is_idempotent() {
    let mut resource = Resource::new("test", "thing");
    resource.set_attr("name", string_value("v1"));

    let first =
        apply_desired_normalization(resource, &[], &NoopNormalizer, &[], &SchemaRegistry::new())
            .await;
    let second = apply_desired_normalization(
        first.as_resource().clone(),
        &[],
        &NoopNormalizer,
        &[],
        &SchemaRegistry::new(),
    )
    .await;

    assert_eq!(first.as_resource(), second.as_resource());
}
