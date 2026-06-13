use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::executor::normalized::apply_desired_normalization;
use crate::parser::ProviderConfig;
use crate::provider::{
    BoxFuture, NoopNormalizer, ProviderFactory, ProviderNormalizer, ProviderResult, ready_noop,
};
use crate::resource::{ConcreteValue, Resource, ResourceId, State, Value};
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
        ResourceSchema::new("thing").attribute(AttributeSchema::new(
            "subjects",
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

    crate::executor::normalized::apply_desired_normalization_in_place(
        &mut resources,
        &[],
        &normalizer,
        &[],
        &schemas,
    )
    .await;

    assert_eq!(
        resources[0].get_attr("subjects"),
        Some(&string_value("one")),
        "in-place desired normalization must leave canonicalize to the caller so plan code can run it before strip/restore"
    );
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
