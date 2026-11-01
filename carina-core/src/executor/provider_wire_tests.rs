//! carina#4212: check the complete payload after host-owned desired rewrites.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::binding_index::ResolvedBindings;
use crate::executor::{
    ModuleConstraintGate, ProviderConfigPreparationError, ProviderPreparationContext,
    ProviderPreparationError, prepare_create_request, prepare_provider_ready_config,
    prepare_provider_ready_data_source, prepare_update_request,
};
use crate::parser::{ExpandedProviderConfig, ResolvedProviderConfigs};
use crate::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, NoopNormalizer, ProviderFactory,
    ProviderInitialization, ProviderReadyConfig, ProviderResult,
};
use crate::resource::{
    ConcreteValue, DataSource, ParsedIgnorePath, ParsedIgnorePathSegment, Resource, State, Value,
};
use crate::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

fn duration(seconds: u64) -> Value {
    Value::Concrete(ConcreteValue::Duration(Duration::from_secs(seconds)))
}

fn assert_duration_error(
    error: &ProviderPreparationError,
    attribute: &str,
    path: &[crate::protocol_convert::WirePathSegment],
) {
    let ProviderPreparationError::Wire(error) = error else {
        panic!("expected a typed wire failure: {error}")
    };
    assert_eq!(error.attribute(), attribute);
    assert_eq!(error.path(), path);
    assert!(matches!(
        error.conversion_error(),
        crate::protocol_convert::ProtocolConvertError::DurationOutOfRange
    ));
    assert!(std::error::Error::source(error).is_none());
    let message = error.to_string();
    assert_eq!(
        message.matches("duration in seconds does not fit").count(),
        1
    );
}

fn tags(value: Value) -> Value {
    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
        "unrelated".into(),
        value,
    )])))
}

#[tokio::test]
async fn wire_error_renders_a_top_level_attribute_once() {
    let resource = Resource::with_provider("test", "sqs.Queue", "queue", None)
        .with_attribute("unread", duration(u64::MAX));
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = SchemaRegistry::new();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &[], &NoopNormalizer, &[], &schemas);
    let error = prepare_create_request(resource, &context)
        .await
        .unwrap_err();
    assert_eq!(error.to_string().matches("`unread`").count(), 1);
}

#[tokio::test]
async fn wire_error_paths_quote_non_identifier_map_keys() {
    for (key, expected) in [
        ("a.b", r#"tags["a.b"]"#),
        ("a\"b", r#"tags["a\"b"]"#),
        ("", r#"tags[""]"#),
        ("0", r#"tags["0"]"#),
        ("with space", r#"tags["with space"]"#),
        ("ordinary_key", "tags.ordinary_key"),
    ] {
        let resource = Resource::with_provider("test", "sqs.Queue", "queue", None).with_attribute(
            "tags",
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                key.into(),
                duration(u64::MAX),
            )]))),
        );
        let bindings = ResolvedBindings::default();
        let gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = SchemaRegistry::new();
        let context =
            ProviderPreparationContext::new(&bindings, &gate, &[], &NoopNormalizer, &[], &schemas);
        let ProviderPreparationError::Wire(error) = prepare_create_request(resource, &context)
            .await
            .unwrap_err()
        else {
            panic!("expected wire error")
        };
        assert_eq!(error.attribute_path(), expected);
        assert!(error.to_string().contains(expected));
    }
}

fn config(default_tag: Value) -> ExpandedProviderConfig {
    ExpandedProviderConfig {
        name: "test".into(),
        attributes: Default::default(),
        default_tags: indexmap::IndexMap::from([("unrelated".into(), default_tag)]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    }
}

fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        PatternResourceSchema::new("sqs.Queue")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

#[tokio::test]
async fn default_tag_duration_boundary_is_signed_i64_seconds() {
    use carina_provider_protocol::types::Value as Wire;
    let max = u64::try_from(i64::MAX).unwrap();
    for seconds in [max, max + 1] {
        let resource = Resource::with_provider("test", "sqs.Queue", "queue", None);
        let bindings = ResolvedBindings::default();
        let gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = schemas();
        let configs = [config(duration(seconds))];
        let context = ProviderPreparationContext::new(
            &bindings,
            &gate,
            &configs,
            &NoopNormalizer,
            &[],
            &schemas,
        );
        let result = prepare_create_request(resource, &context).await;
        if seconds == max {
            let request = result.unwrap();
            assert_eq!(
                request
                    .resource()
                    .wire_attributes()
                    .get("tags")
                    .unwrap()
                    .value(),
                &Wire::Map(BTreeMap::from([("unrelated".into(), Wire::Int(i64::MAX))]))
            );
        } else {
            assert_duration_error(
                &result.unwrap_err(),
                "tags",
                &[crate::protocol_convert::WirePathSegment::Key(
                    "unrelated".into(),
                )],
            );
        }
    }
}

#[tokio::test]
async fn native_create_rejects_a_non_wire_default_tag_before_dispatch() {
    let resource = Resource::with_provider("test", "sqs.Queue", "queue", None);
    let id = resource.id.clone();
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let configs = [config(duration(u64::MAX))];
    let schemas = schemas();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &configs, &NoopNormalizer, &[], &schemas);
    let error = prepare_create_request(resource, &context)
        .await
        .expect_err("a merged default tag must be checked before native create");
    let rendered = error.to_string();
    assert_duration_error(
        &error,
        "tags",
        &[crate::protocol_convert::WirePathSegment::Key(
            "unrelated".into(),
        )],
    );
    assert!(rendered.contains(&id.to_string()), "{rendered}");
    assert!(rendered.contains("tags.unrelated"), "{rendered}");
    assert!(
        rendered.contains("duration in seconds does not fit"),
        "{rendered}"
    );
    assert!(!rendered.contains(&u64::MAX.to_string()), "{rendered}");
}

#[tokio::test]
async fn native_update_checks_default_tags_outside_changed_attributes() {
    let resource = Resource::with_provider("test", "sqs.Queue", "queue", None);
    let from = State::existing(resource.id.clone(), HashMap::new());
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let configs = [config(duration(u64::MAX))];
    let schemas = schemas();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &configs, &NoopNormalizer, &[], &schemas);
    let error = prepare_update_request(resource, from, &["name".into()], &context)
        .await
        .expect_err("checking only the patch would miss an unrelated default tag");
    assert_duration_error(
        &error,
        "tags",
        &[crate::protocol_convert::WirePathSegment::Key(
            "unrelated".into(),
        )],
    );
}

#[tokio::test]
async fn native_update_checks_observed_ignored_leaves_after_patch_substitution() {
    let mut resource = Resource::with_provider("test", "sqs.Queue", "queue", None).with_attribute(
        "tags",
        tags(Value::Concrete(ConcreteValue::String("valid".into()))),
    );
    resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        ParsedIgnorePathSegment::Attribute("tags".into()),
        ParsedIgnorePathSegment::MapKey("unrelated".into()),
    ])];
    let from = State::existing(
        resource.id.clone(),
        HashMap::from([("tags".into(), tags(duration(u64::MAX)))]),
    );
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = schemas();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &[], &NoopNormalizer, &[], &schemas);
    let error = prepare_update_request(resource, from, &["tags".into()], &context)
        .await
        .expect_err("an observed leaf can become non-wire after desired preparation");
    assert_duration_error(
        &error,
        "tags",
        &[crate::protocol_convert::WirePathSegment::Key(
            "unrelated".into(),
        )],
    );
}

#[test]
fn native_data_source_checks_even_unschematized_attributes() {
    let mut data = DataSource::pending_with_provider("test", "lookup", None);
    data.id = crate::resource::ResourceId::with_provider_identity("test", "lookup", "lookup", None);
    data.attributes
        .insert("unread".into(), tags(duration(u64::MAX)));
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = SchemaRegistry::new();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &[], &NoopNormalizer, &[], &schemas);
    let error = prepare_provider_ready_data_source(data, &context)
        .expect_err("a native data-source provider must never receive a non-wire value");
    assert_duration_error(
        &error,
        "unread",
        &[crate::protocol_convert::WirePathSegment::Key(
            "unrelated".into(),
        )],
    );
}

struct ConfigFactory(AtomicUsize);

impl ProviderFactory for ConfigFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "wire boundary fixture"
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(BTreeMap::new()).unwrap()
    }
    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &ProviderReadyConfig,
        _: crate::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        panic!("invalid configuration must not initialize a provider")
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        Vec::new()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }
}

#[test]
fn native_factory_rejects_non_wire_config_before_semantic_validation() {
    let factory = ConfigFactory(AtomicUsize::new(0));
    let mut config = config(duration(1));
    config.attributes.insert(
        "extra".into(),
        tags(Value::Concrete(ConcreteValue::Float(f64::NAN))),
    );
    let configs = ResolvedProviderConfigs::from_test_configs(vec![config]).unwrap();
    let error = prepare_provider_ready_config(&factory, configs.iter().next().unwrap())
        .expect_err("native factory validation must not receive a non-wire configuration");
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    let rendered = error.to_string();
    assert!(rendered.contains("extra.unrelated"), "{rendered}");
    assert!(rendered.contains("non-finite float"), "{rendered}");
    assert!(!rendered.contains("NaN"), "{rendered}");
    let ProviderConfigPreparationError::Wire(error) = error else {
        panic!("expected typed config wire failure")
    };
    assert_eq!(
        error.target(),
        &crate::provider::ProviderWireTarget::Configuration {
            provider: "test".into()
        }
    );
    assert!(matches!(
        error.conversion_error(),
        crate::protocol_convert::ProtocolConvertError::NonFiniteFloat
    ));
}

#[tokio::test]
async fn checked_projection_preserves_native_scalars_and_late_secret_provenance() {
    use crate::resource::{DeferredValue, EnumValueResolver};
    use crate::schema::{CompiledAttributeType, TypeIdentity};
    use carina_provider_protocol::types::Value as Wire;
    let enum_type = CompiledAttributeType::enum_(
        TypeIdentity::bare("Mode"),
        Some(vec!["active".into()]),
        Vec::new(),
        None,
        None,
    );
    let mode = EnumValueResolver::new(&enum_type)
        .resolve_state_text("active")
        .unwrap();
    let resource = Resource::with_provider("test", "sqs.Queue", "queue", None).with_attribute(
        "unread",
        Value::Concrete(ConcreteValue::List(vec![
            duration(i64::MAX as u64),
            Value::Concrete(ConcreteValue::CanonicalEnum(mode)),
        ])),
    );
    let configs = [config(Value::Deferred(DeferredValue::Secret(Box::new(
        Value::Concrete(ConcreteValue::String("private-late-tag".into())),
    ))))];
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = schemas();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &configs, &NoopNormalizer, &[], &schemas);
    let request = prepare_create_request(resource, &context).await.unwrap();
    let ready = request.resource();
    assert_eq!(
        ready.wire_attributes().get("unread").unwrap().value(),
        &Wire::List(vec![Wire::Int(i64::MAX), Wire::String("active".into())])
    );
    let Value::Concrete(ConcreteValue::List(original)) = &ready.as_resource().attributes["unread"]
    else {
        panic!()
    };
    assert!(matches!(
        original[0],
        Value::Concrete(ConcreteValue::Duration(_))
    ));
    assert!(matches!(
        original[1],
        Value::Concrete(ConcreteValue::CanonicalEnum(_))
    ));
    let Value::Concrete(ConcreteValue::Map(tags)) = &ready.as_resource().attributes["tags"] else {
        panic!()
    };
    assert!(matches!(
        tags["unrelated"],
        Value::Deferred(DeferredValue::Secret(_))
    ));
    assert!(ready.default_tag_keys.contains("unrelated"));
    assert!(!format!("{request:?}").contains("private-late-tag"));
}

#[tokio::test]
async fn nested_wire_failures_retain_list_indices_and_never_values() {
    use crate::protocol_convert::WirePathSegment::{Index, Key};
    let resource = Resource::with_provider("test", "sqs.Queue", "queue", None).with_attribute(
        "unread",
        tags(Value::Concrete(ConcreteValue::List(vec![duration(
            u64::MAX,
        )]))),
    );
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = schemas();
    let context =
        ProviderPreparationContext::new(&bindings, &gate, &[], &NoopNormalizer, &[], &schemas);
    let error = prepare_create_request(resource, &context)
        .await
        .unwrap_err();
    assert_duration_error(&error, "unread", &[Key("unrelated".into()), Index(0)]);
    assert!(error.to_string().contains("unread.unrelated[0]"));
    assert!(std::error::Error::source(&error).is_none());
}
