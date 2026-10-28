use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::executor::normalized::{apply_desired_normalization, apply_desired_normalization_slice};
use crate::parser::ProviderConfig;
use crate::provider::{
    BoxFuture, NoopNormalizer, ProviderError, ProviderNormalizer, ProviderResult, ready_noop,
};
use crate::resource::{
    ConcreteValue, DeferredValue, InterpolationPart, Resource, UnknownReason, Value,
};
use crate::schema::SchemaRegistry;

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
    }
}

fn string_value(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string().into()))
}

struct AttributeReturningNormalizer {
    name: &'static str,
    value: Value,
}

impl ProviderNormalizer for AttributeReturningNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            resources[0].set_attr(self.name, self.value.clone());
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }
}

#[tokio::test]
async fn normalized_resource_rejects_block_aliases_returned_by_provider_normalizer() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("thing").attribute(
            AttributeSchema::optional(
                "rules",
                AttributeType::list(AttributeType::struct_(
                    "Rule",
                    vec![StructField::optional("action", AttributeType::string()).build()],
                )),
            )
            .with_block_name("rule")
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let normalizer = AttributeReturningNormalizer {
        name: "rule",
        value: Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "action".to_string(),
                string_value("allow"),
            )])),
        )])),
    };

    let error = apply_desired_normalization(
        Resource::with_provider("test", "thing", "example", None),
        &[],
        &normalizer,
        &schemas,
    )
    .await
    .expect_err("provider normalizers must return canonical attribute names");

    assert!(matches!(error, ProviderError::InvalidInput(_)));
    let message = error.to_string();
    assert!(message.contains("block-name alias `rule` reached the provider boundary"));
    assert!(message.contains("canonical attribute `rules`"));
    assert!(!message.contains("provider normalizer returned"));
}

#[tokio::test]
async fn normalized_resource_rejects_nested_block_aliases_returned_by_provider_normalizer() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let condition = AttributeType::struct_(
        "Condition",
        vec![StructField::optional("value", AttributeType::string()).build()],
    );
    let rule = AttributeType::struct_(
        "Rule",
        vec![
            StructField::optional("conditions", AttributeType::list(condition))
                .with_block_name("condition")
                .build(),
        ],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("thing")
            .attribute(AttributeSchema::optional("rules", AttributeType::list(rule)).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let normalizer = AttributeReturningNormalizer {
        name: "rules",
        value: Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "condition".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Map(IndexMap::from([(
                        "value".to_string(),
                        string_value("ready"),
                    )])),
                )])),
            )])),
        )])),
    };

    let error = apply_desired_normalization(
        Resource::with_provider("test", "thing", "example", None),
        &[],
        &normalizer,
        &schemas,
    )
    .await
    .expect_err("nested provider-normalizer aliases must be rejected");

    assert!(matches!(error, ProviderError::InvalidInput(_)));
    assert!(error.to_string().contains("block-name alias `condition`"));
    assert!(error.to_string().contains("`rules[0].condition`"));
    assert!(
        error
            .to_string()
            .contains("canonical attribute `conditions`")
    );
}

#[tokio::test]
async fn normalization_error_stops_later_stages() {
    struct FailingNormalizer;

    impl ProviderNormalizer for FailingNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                assert!(
                    resources[0].get_attr("peer").is_some(),
                    "in-process normalizers must receive ref-bearing attributes"
                );
                Err(ProviderError::internal("normalization failed"))
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }
    }

    let mut default_tags = IndexMap::new();
    default_tags.insert("ManagedBy".to_string(), string_value("carina"));
    let mut resources = vec![Resource::with_provider("test", "thing", "example", None)];
    let peer_ref = Value::resource_ref("peer", "identifier", vec![]);
    resources[0].set_attr("peer", peer_ref.clone());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (crate::schema::ResourceSchema::new("thing").attribute(
            crate::schema::AttributeSchema::optional(
                "tags",
                crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let error = apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &FailingNormalizer,
        &schemas,
    )
    .await
    .expect_err("normalizer error must propagate");

    assert_eq!(error.to_string(), "normalization failed");
    assert!(
        resources[0].get_attr("tags").is_none(),
        "host default-tag merging must not run after a normalizer error"
    );
    assert_eq!(
        resources[0].get_attr("peer"),
        Some(&peer_ref),
        "an in-process normalizer error must leave untouched attributes intact"
    );
}

#[tokio::test]
async fn invalid_default_tag_fails_normalization_without_mutating_resources() {
    let default_tags = IndexMap::from([
        ("ManagedBy".to_string(), string_value("carina")),
        (
            "InvalidNumber".to_string(),
            Value::Concrete(ConcreteValue::Float(f64::NAN)),
        ),
    ]);
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr("name", string_value("unchanged"));
    let mut resources = vec![resource];
    let original = resources.clone();
    let schemas = order_schema();

    let error = apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &NoopNormalizer,
        &schemas,
    )
    .await
    .expect_err("a non-finite default tag must fail normalization");

    let rendered = error.to_string();
    assert!(
        rendered
            .starts_with("failed to encode default tag: key 'InvalidNumber' of provider 'test': "),
        "{rendered}"
    );
    assert!(rendered.contains("provider 'test'"), "{rendered}");
    assert!(rendered.contains("InvalidNumber"), "{rendered}");
    assert!(
        error.detail().cause().is_some(),
        "default-tag failure must retain its typed terminal cause"
    );
    let leaf = "cannot serialize at WASM provider boundary: non-finite float NaN";
    assert_eq!(rendered.matches(leaf).count(), 1, "{rendered}");
    assert_eq!(
        resources, original,
        "default-tag validation must finish before any resource is mutated"
    );
}

#[tokio::test]
async fn unresolved_enum_default_tag_fails_before_mutating_resources() {
    let default_tags = IndexMap::from([(
        "Mode".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier("test.Mode.allow")),
    )]);
    let mut resources = vec![Resource::with_provider("test", "thing", "example", None)];
    let original = resources.clone();

    let error = apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &NoopNormalizer,
        &order_schema(),
    )
    .await
    .expect_err("an unresolved enum default tag must not enter desired resources");

    let rendered = error.to_string();
    let leaf =
        "cannot serialize at WASM provider boundary: unresolved enum identifier test.Mode.allow";
    assert_eq!(rendered.matches(leaf).count(), 1, "{rendered}");
    assert_eq!(resources, original);
}

fn order_schema() -> SchemaRegistry {
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, enum_identity,
    };

    let mode = AttributeType::enum_(
        enum_identity("Mode", Some("test.thing")),
        Some(vec!["friendly".to_string()]),
        Vec::new(),
        None,
        None,
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("thing")
            .attribute(
                AttributeSchema::optional(
                    "subjects",
                    AttributeType::union(vec![
                        AttributeType::string(),
                        AttributeType::list(AttributeType::string()),
                    ]),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "ref_subjects",
                    AttributeType::union(vec![
                        AttributeType::string(),
                        AttributeType::list(AttributeType::string()),
                    ]),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(AttributeSchema::optional("mode", mode.clone()).build())
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![
                            StructField::optional("mode", mode).build(),
                            StructField::optional("peer", AttributeType::string()).build(),
                        ],
                    ),
                )
                .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

#[tokio::test]
async fn desired_normalization_runs_stages_in_order() {
    struct OrderNormalizer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderNormalizer for OrderNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                let subjects = resources[0].get_attr("subjects").cloned();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("normalize_desired:{subjects:?}"));
                // Model the real WASM ABI result: provider normalization
                // returns the enum's API spelling as an ordinary string.
                resources[0].set_attr("mode", string_value("friendly"));
                resources[0].set_attr(
                    "tags",
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "Explicit".to_string(),
                        string_value("resource"),
                    )]))),
                );
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
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
    let schemas = order_schema();

    let normalized = apply_desired_normalization(
        resource,
        &[provider_config(default_tags)],
        &normalizer,
        &schemas,
    )
    .await
    .unwrap();

    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![format!(
            "normalize_desired:{:?}",
            Some(Value::Concrete(ConcreteValue::StringList(vec![
                "one".into()
            ])))
        )],
        "canonicalization must run before normalize_desired"
    );
    assert_eq!(
        normalized.as_resource().get_attr("tags"),
        Some(&Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("Explicit".to_string(), string_value("resource")),
            ("ManagedBy".to_string(), string_value("carina")),
        ])))),
        "host default-tag merging must run after normalize_desired"
    );
    assert_eq!(
        normalized.as_resource().get_attr("mode"),
        Some(&Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::enum_identity("Mode", Some("test.thing")),
                "friendly",
            ),
        ))),
        "the post-normalizer schema pass must lift the API string"
    );
}

#[tokio::test]
async fn desired_normalization_lifts_guest_string_alias_to_api_enum() {
    struct StringAliasNormalizer;

    impl ProviderNormalizer for StringAliasNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                resources[0].set_attr("mode", string_value("allow"));
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }
    }

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (crate::schema::ResourceSchema::new("thing").attribute(
            crate::schema::AttributeSchema::optional(
                "mode",
                crate::schema::AttributeType::enum_(
                    crate::schema::enum_identity("Mode", Some("test.thing")),
                    Some(vec!["Allow".to_string(), "Deny".to_string()]),
                    vec![("Allow".to_string(), "allow".to_string())],
                    None,
                    None,
                ),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let resource = Resource::with_provider("test", "thing", "example", None);

    let normalized = apply_desired_normalization(resource, &[], &StringAliasNormalizer, &schemas)
        .await
        .unwrap();

    assert!(matches!(
        normalized.as_resource().get_attr("mode"),
        Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
            if value.api_value() == "Allow"
    ));
}

#[tokio::test]
async fn desired_normalization_rejects_guest_dotted_string_without_shape_stripping() {
    struct DottedStringNormalizer;

    impl ProviderNormalizer for DottedStringNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                resources[0].set_attr("mode", string_value("test.thing.Mode.allow"));
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }
    }

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (crate::schema::ResourceSchema::new("thing").attribute(
            crate::schema::AttributeSchema::optional(
                "mode",
                crate::schema::AttributeType::enum_(
                    crate::schema::enum_identity("Mode", Some("test.thing")),
                    Some(vec!["Allow".to_string(), "Deny".to_string()]),
                    vec![("Allow".to_string(), "allow".to_string())],
                    None,
                    None,
                ),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let resource = Resource::with_provider("test", "thing", "example", None);

    let error = apply_desired_normalization(resource, &[], &DottedStringNormalizer, &schemas)
        .await
        .expect_err("guest strings must resolve by literal membership, never dotted shape");

    assert!(error.to_string().contains("test.thing.Mode.allow"));
    assert!(error.detail().cause().is_some());
}

#[tokio::test]
async fn desired_normalization_merges_non_empty_provider_default_tags() {
    let resource = Resource::with_provider("test", "thing", "example", None);
    let mut default_tags = IndexMap::new();
    default_tags.insert("ManagedBy".to_string(), string_value("carina"));
    let schemas = order_schema();

    let normalized = apply_desired_normalization(
        resource,
        &[
            provider_config(IndexMap::new()),
            provider_config(default_tags),
        ],
        &NoopNormalizer,
        &schemas,
    )
    .await
    .unwrap();

    assert_eq!(
        normalized.as_resource().get_attr("tags"),
        Some(&Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "ManagedBy".to_string(),
            string_value("carina"),
        )])))),
        "the non-empty provider config must supply default tags"
    );
}

#[tokio::test]
async fn desired_normalization_merges_default_tags_into_ref_bearing_tags() {
    let peer_ref = Value::resource_ref("peer", "identifier", vec![]);
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr(
        "tags",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("Name".to_string(), string_value("example")),
            ("Peer".to_string(), peer_ref.clone()),
        ]))),
    );
    let default_tags = IndexMap::from([("ManagedBy".to_string(), string_value("carina"))]);

    let normalized = apply_desired_normalization(
        resource,
        &[provider_config(default_tags)],
        &NoopNormalizer,
        &order_schema(),
    )
    .await
    .unwrap();

    assert_eq!(
        normalized.as_resource().get_attr("tags"),
        Some(&Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("Name".to_string(), string_value("example")),
            ("Peer".to_string(), peer_ref),
            ("ManagedBy".to_string(), string_value("carina")),
        ])))),
        "host default-tag merging must see the full ref-bearing tags map"
    );
    assert!(
        normalized
            .as_resource()
            .default_tag_keys
            .contains("ManagedBy"),
        "default-tag metadata must include the key actually merged into tags"
    );
    assert!(
        normalized
            .as_resource()
            .attributes
            .keys()
            .all(|key| !key.starts_with('_')),
        "host metadata must not enter normalized provider attributes"
    );
}

#[tokio::test]
async fn desired_normalization_resolves_enum_alias_nested_with_resource_ref() {
    let peer_ref = Value::resource_ref("peer", "identifier", vec![]);
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr(
        "settings",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "mode".to_string(),
                Value::Concrete(ConcreteValue::enum_identifier("Mode.friendly")),
            ),
            ("peer".to_string(), peer_ref.clone()),
        ]))),
    );
    let normalized = apply_desired_normalization(resource, &[], &NoopNormalizer, &order_schema())
        .await
        .unwrap();

    let Some(Value::Concrete(ConcreteValue::Map(settings))) =
        normalized.as_resource().get_attr("settings")
    else {
        panic!("settings must remain a map");
    };
    assert!(matches!(
        settings.get("mode"),
        Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)))
            if canonical.identity().to_string() == "test.thing.Mode"
                && canonical.api_value() == "friendly"
    ));
    assert_eq!(settings.get("peer"), Some(&peer_ref));
}

#[tokio::test]
async fn desired_normalization_scopes_default_tags_to_provider_instance() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut default_tags = IndexMap::new();
    default_tags.insert("Inst".to_string(), string_value("default"));
    let default_config = provider_config(default_tags);

    let mut second_tags = IndexMap::new();
    second_tags.insert("Inst".to_string(), string_value("second"));
    second_tags.insert("Only".to_string(), string_value("second"));
    let mut second_config = provider_config(second_tags);
    second_config.binding = Some("second".to_string());

    let mut resources = vec![
        Resource::with_provider("test", "thing", "default", None),
        Resource::with_provider("test", "thing", "named", Some("second".to_string())),
    ];
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("thing").attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    apply_desired_normalization_slice(
        &mut resources,
        &[default_config, second_config],
        &NoopNormalizer,
        &schemas,
    )
    .await
    .unwrap();

    let actual_tags: Vec<_> = resources
        .iter()
        .map(|resource| match resource.get_attr("tags") {
            Some(Value::Concrete(ConcreteValue::Map(tags))) => tags.clone(),
            other => panic!("expected concrete tags map, got {other:?}"),
        })
        .collect();
    assert_eq!(
        actual_tags,
        vec![
            IndexMap::from([("Inst".to_string(), string_value("default"))]),
            IndexMap::from([
                ("Inst".to_string(), string_value("second")),
                ("Only".to_string(), string_value("second")),
            ]),
        ],
        "each resource must receive default tags only from its routed provider instance"
    );
}

#[tokio::test]
async fn desired_normalization_skips_tags_that_are_not_maps_of_strings() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let default_tags = IndexMap::from([("ManagedBy".to_string(), string_value("carina"))]);
    let mut resources = vec![
        Resource::with_provider("test", "list_tags", "list", None),
        Resource::with_provider("test", "struct_tags", "struct", None),
        Resource::with_provider("test", "numeric_map_tags", "map", None),
    ];
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("list_tags").attribute(
            AttributeSchema::optional("tags", AttributeType::list(AttributeType::string())).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        (ResourceSchema::new("struct_tags").attribute(
            AttributeSchema::optional(
                "tags",
                AttributeType::struct_(
                    "Tags",
                    vec![StructField::optional("owner", AttributeType::string()).build()],
                ),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        (ResourceSchema::new("numeric_map_tags").attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::int())).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &NoopNormalizer,
        &schemas,
    )
    .await
    .unwrap();

    for resource in resources {
        assert!(
            resource.get_attr("tags").is_none(),
            "default tags must not be merged into {}",
            resource.id
        );
    }
}

#[tokio::test]
async fn apply_desired_normalization_passes_deferred_values_to_in_process_normalizers() {
    struct InspectingNormalizer;

    impl ProviderNormalizer for InspectingNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                assert!(matches!(
                    resources[0].get_attr("role_arn"),
                    Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
                ));
                assert!(matches!(
                    resources[0].get_attr("pending"),
                    Some(Value::Deferred(DeferredValue::Unknown(_)))
                ));
                resources[0].set_attr(
                    "observed_full_resource",
                    Value::Concrete(ConcreteValue::Bool(true)),
                );
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }
    }

    let deferred = Value::resource_ref("role", "arn", vec![]);
    let mut resource = Resource::new("test", "thing");
    resource.set_attr("role_arn", deferred.clone());
    resource.set_attr(
        "pending",
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
    );

    let normalized =
        apply_desired_normalization(resource, &[], &InspectingNormalizer, &SchemaRegistry::new())
            .await
            .unwrap();

    assert_eq!(
        normalized.as_resource().get_attr("role_arn"),
        Some(&deferred),
        "in-process normalization must preserve deferred resource refs"
    );
    assert!(matches!(
        normalized.as_resource().get_attr("pending"),
        Some(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ForValue
        )))
    ));
    assert_eq!(
        normalized.as_resource().get_attr("observed_full_resource"),
        Some(&Value::Concrete(ConcreteValue::Bool(true)))
    );
}

#[tokio::test]
async fn desired_normalization_slice_canonicalizes_and_runs_stages_on_full_resources() {
    struct FullResourceNormalizer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderNormalizer for FullResourceNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                assert!(
                    matches!(
                        resources[0].get_attr("ref_subjects"),
                        Some(Value::Deferred(DeferredValue::Interpolation(_)))
                    ),
                    "in-process normalize_desired must receive ref-bearing attributes"
                );
                self.calls
                    .lock()
                    .unwrap()
                    .push("normalize_desired".to_string());
                resources[0].set_attr("mode", string_value("friendly"));
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }
    }

    let ref_interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("role:".into()),
        InterpolationPart::Expr(Value::resource_ref("role", "arn", vec![])),
    ]));
    let mut resource = Resource::with_provider("test", "thing", "example", None);
    resource.set_attr("subjects", string_value("one"));
    resource.set_attr("ref_subjects", ref_interpolation.clone());

    let calls = Arc::new(Mutex::new(Vec::new()));
    let normalizer = FullResourceNormalizer {
        calls: Arc::clone(&calls),
    };
    let mut default_tags = IndexMap::new();
    default_tags.insert("ManagedBy".to_string(), string_value("carina"));
    let schemas = order_schema();
    let mut resources = vec![resource];

    apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &normalizer,
        &schemas,
    )
    .await
    .unwrap();

    assert_eq!(
        calls.lock().unwrap().clone(),
        vec!["normalize_desired".to_string()]
    );
    assert_eq!(
        resources[0].get_attr("subjects"),
        Some(&Value::Concrete(ConcreteValue::StringList(vec![
            "one".into()
        ]))),
        "slice wrapper must canonicalize before running provider stages"
    );
    assert_eq!(
        resources[0].get_attr("ref_subjects"),
        Some(&ref_interpolation),
        "the in-process normalizer must retain the ref-bearing interpolation"
    );
    assert_eq!(
        resources[0].get_attr("tags"),
        Some(&Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "ManagedBy".to_string(),
            string_value("carina"),
        )])))),
        "slice wrapper must run host default-tag merging on the full resource"
    );
    assert!(
        matches!(
            resources[0].get_attr("mode"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)))
                if canonical.identity().to_string() == "test.thing.Mode"
                    && canonical.api_value() == "friendly"
        ),
        "slice wrapper must re-canonicalize provider-returned enum text"
    );
}

#[tokio::test]
async fn apply_desired_normalization_is_idempotent() {
    let mut resource = Resource::new("test", "thing");
    resource.set_attr("name", string_value("v1"));

    let first = apply_desired_normalization(resource, &[], &NoopNormalizer, &SchemaRegistry::new())
        .await
        .unwrap();
    let second = apply_desired_normalization(
        first.as_resource().clone(),
        &[],
        &NoopNormalizer,
        &SchemaRegistry::new(),
    )
    .await
    .unwrap();

    assert_eq!(first.as_resource(), second.as_resource());
}
