use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::executor::normalized::{apply_desired_normalization, apply_desired_normalization_slice};
use crate::parser::ProviderConfig;
use crate::provider::{
    BoxFuture, NoopNormalizer, ProviderError, ProviderFactory, ProviderNormalizer, ProviderResult,
    ready_noop,
};
use crate::resource::{
    ConcreteValue, DeferredValue, InterpolationPart, Resource, ResourceId, State, UnknownReason,
    Value,
};
use crate::schema::SchemaRegistry;
use crate::value::SerializationError;

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
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
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
        crate::schema::ResourceSchema::new("thing").attribute(crate::schema::AttributeSchema::new(
            "tags",
            crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
        )),
    );

    let error = apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &FailingNormalizer,
        &[],
        &schemas,
    )
    .await
    .expect_err("normalizer error must propagate");

    assert_eq!(error.message(), "normalization failed");
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
        &[],
        &schemas,
    )
    .await
    .expect_err("a non-finite default tag must fail normalization");

    let rendered = error.to_string();
    assert!(rendered.contains("provider 'test'"), "{rendered}");
    assert!(rendered.contains("InvalidNumber"), "{rendered}");
    let source = std::error::Error::source(&error)
        .expect("default-tag failure must retain its serialization source");
    assert!(
        matches!(
            source.downcast_ref::<SerializationError>(),
            Some(SerializationError::NonFiniteFloat { value, .. }) if value.is_nan()
        ),
        "unexpected source: {source:?}"
    );
    assert_eq!(
        resources, original,
        "default-tag validation must finish before any resource is mutated"
    );
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
        _config: &crate::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn crate::provider::Provider>>> {
        unreachable!("test factory does not create providers")
    }

    fn schemas(&self) -> Vec<crate::schema::ResourceSchema> {
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
        vec![
            ResourceSchema::new("thing")
                .attribute(AttributeSchema::new("mode", mode.clone()))
                .attribute(AttributeSchema::new(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![
                            StructField::new("mode", mode),
                            StructField::new("peer", AttributeType::string()),
                        ],
                    ),
                )),
        ]
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

struct RecordingAliasFactory {
    calls: Arc<Mutex<Vec<String>>>,
}

impl ProviderFactory for RecordingAliasFactory {
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
        _config: &crate::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn crate::provider::Provider>>> {
        unreachable!("test factory does not create providers")
    }

    fn schemas(&self) -> Vec<crate::schema::ResourceSchema> {
        self.calls
            .lock()
            .unwrap()
            .push("resolve_enum_aliases".to_string());
        AliasFactory.schemas()
    }

    fn get_enum_alias_reverse(
        &self,
        resource_type: &str,
        attr_name: &str,
        value: &str,
    ) -> Option<String> {
        AliasFactory.get_enum_alias_reverse(resource_type, attr_name, value)
    }
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
            ))
            .attribute(AttributeSchema::new(
                "tags",
                AttributeType::map(AttributeType::string()),
            ))
            .attribute(AttributeSchema::new("mode", mode.clone()))
            .attribute(AttributeSchema::new(
                "settings",
                AttributeType::struct_(
                    "Settings",
                    vec![
                        StructField::new("mode", mode),
                        StructField::new("peer", AttributeType::string()),
                    ],
                ),
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
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
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
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RecordingAliasFactory {
        calls: Arc::clone(&calls),
    })];
    let schemas = order_schema();

    let normalized = apply_desired_normalization(
        resource,
        &[provider_config(default_tags)],
        &normalizer,
        &factories,
        &schemas,
    )
    .await
    .unwrap();

    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![
            format!(
                "normalize_desired:{:?}",
                Some(Value::Concrete(ConcreteValue::StringList(vec![
                    "one".into()
                ])))
            ),
            "resolve_enum_aliases".to_string(),
        ],
        "canonicalize must run before normalize_desired, and enum resolution must run after it"
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
        Some(&string_value("friendly")),
        "the typed alias pass must not reclassify the normalizer's API string"
    );
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
        &[],
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
        &[],
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
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];

    let normalized =
        apply_desired_normalization(resource, &[], &NoopNormalizer, &factories, &order_schema())
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
        ResourceSchema::new("thing").attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::string()),
        )),
    );

    apply_desired_normalization_slice(
        &mut resources,
        &[default_config, second_config],
        &NoopNormalizer,
        &[],
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
        ResourceSchema::new("list_tags").attribute(AttributeSchema::new(
            "tags",
            AttributeType::list(AttributeType::string()),
        )),
    );
    schemas.insert(
        "test",
        ResourceSchema::new("struct_tags").attribute(AttributeSchema::new(
            "tags",
            AttributeType::struct_(
                "Tags",
                vec![StructField::new("owner", AttributeType::string())],
            ),
        )),
    );
    schemas.insert(
        "test",
        ResourceSchema::new("numeric_map_tags").attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::int()),
        )),
    );

    apply_desired_normalization_slice(
        &mut resources,
        &[provider_config(default_tags)],
        &NoopNormalizer,
        &[],
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
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
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

    let normalized = apply_desired_normalization(
        resource,
        &[],
        &InspectingNormalizer,
        &[],
        &SchemaRegistry::new(),
    )
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
                resources[0].set_attr(
                    "mode",
                    Value::Concrete(ConcreteValue::enum_identifier("Mode.friendly")),
                );
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
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
        "slice wrapper must run enum alias resolution after provider stages"
    );
}

#[tokio::test]
async fn apply_desired_normalization_is_idempotent() {
    let mut resource = Resource::new("test", "thing");
    resource.set_attr("name", string_value("v1"));

    let first =
        apply_desired_normalization(resource, &[], &NoopNormalizer, &[], &SchemaRegistry::new())
            .await
            .unwrap();
    let second = apply_desired_normalization(
        first.as_resource().clone(),
        &[],
        &NoopNormalizer,
        &[],
        &SchemaRegistry::new(),
    )
    .await
    .unwrap();

    assert_eq!(first.as_resource(), second.as_resource());
}
