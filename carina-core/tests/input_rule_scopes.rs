//! Regression coverage for desired-only rules (carina#4267, #4295 and #3900).
use std::collections::HashMap;

use carina_core::{
    effect::Effect,
    plan::{Plan, PlanErrorKind},
    protocol_convert::{protocol_to_resource_schema, resource_schema_to_protocol},
    provider::AppliedIdentifier,
    resource::{ConcreteValue, ResolvedResource, Resource, State, Value},
    schema::{
        AttributeSchema, AttributeType, InputPath, InputPredicate, InputRule, InputRuleCode,
        InputRuleScope, InputRuleTarget, NonEmptyString, ResourceSchema, SchemaRegistry,
    },
};

#[path = "support/creation.rs"]
#[allow(dead_code)]
mod fixture;

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

fn tag_rule(scope: InputRuleScope) -> InputRule {
    InputRule::new(
        scope,
        InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
        InputRuleCode::new("tags.reserved_prefix").unwrap(),
        InputPredicate::ForbiddenPrefix {
            prefix: NonEmptyString::new("aws:").unwrap(),
            ignore_ascii_case: true,
        },
    )
}

fn schema(scope: InputRuleScope) -> ResourceSchema {
    ResourceSchema::new("test.Tagged")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        )
        .input_rule(tag_rule(scope))
}

fn registry(scope: InputRuleScope) -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry.insert("mock", schema(scope).validate().unwrap());
    registry
}

fn desired(registry: &SchemaRegistry, key: Option<&str>) -> ResolvedResource {
    let mut resource = Resource::with_provider("mock", "test.Tagged", "example", None)
        .with_attribute("name", string("updated"));
    if let Some(key) = key {
        resource = resource.with_attribute(
            "tags",
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                key.into(),
                string("secret-tag-value"),
            )]))),
        );
    }
    ResolvedResource::resolve_raw_fixture(resource, registry).unwrap()
}

fn update(registry: &SchemaRegistry, key: Option<&str>) -> Plan {
    let to = desired(registry, key);
    // CloudFormation's reserved tags are legitimate read-back/state values.
    let from = State::existing(
        to.id.clone(),
        HashMap::from([(
            "tags".into(),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "aws:cloudformation:stack-id".into(),
                string("observed-stack"),
            )]))),
        )]),
    )
    .with_identifier("existing");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from.into()),
        identifier: AppliedIdentifier::new("existing").unwrap(),
        to,
        changed_attributes: vec!["name".into(), "tags".into()],
    });
    plan
}

#[test]
fn every_write_update_checks_authored_keys_case_insensitively_and_never_read_back() {
    let registry = registry(InputRuleScope::EveryWrite);
    for key in ["aws:probe", "AWS:probe", "Aws:probe", "aWs:probe"] {
        let rejected = fixture::finalize(update(&registry, Some(key)))
            .check_inputs(&registry, &HashMap::new())
            .unwrap_err();
        assert!(matches!(&rejected.errors()[0].kind,
            PlanErrorKind::InputRule { path, code, .. }
            if path == "tags" && code == "tags.reserved_prefix"));
        let diagnostic = format!("{:?}", rejected.errors());
        assert!(!diagnostic.contains(key));
        assert!(!diagnostic.contains("secret-tag-value"));
    }
    for key in [None, Some("safe"), Some("xaws:probe")] {
        assert!(
            fixture::finalize(update(&registry, key))
                .check_inputs(&registry, &HashMap::new())
                .is_ok()
        );
    }
}

#[test]
fn create_only_rule_does_not_participate_in_updates() {
    let registry = registry(InputRuleScope::CreateOnly);
    assert!(
        fixture::finalize(update(&registry, Some("aws:probe")))
            .check_inputs(&registry, &HashMap::new())
            .is_ok()
    );
}

#[test]
fn update_unknown_target_retains_pending_obligation_and_fails_closed() {
    let registry = registry(InputRuleScope::EveryWrite);
    let mut to = desired(&registry, None).into_inner();
    to.attributes.insert(
        "tags".into(),
        Value::Deferred(carina_core::resource::DeferredValue::Unknown(
            carina_core::resource::ApplyTimeUnknown::PendingReRead(
                carina_core::resource::PendingReRead::not_found(),
            ),
        )),
    );
    let to = ResolvedResource::resolve_with_schemas(to, &registry).unwrap();
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(State::existing(to.id.clone(), HashMap::new()).with_identifier("existing")),
        identifier: AppliedIdentifier::new("existing").unwrap(),
        to,
        changed_attributes: vec!["tags".into()],
    });
    let checked = fixture::finalize(plan)
        .check_inputs(&registry, &HashMap::new())
        .unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    assert!(!checked.pending_inputs().is_empty());
}

#[test]
fn protocol_round_trip_preserves_scope_map_key_target_and_prefix_case_mode() {
    for scope in [InputRuleScope::CreateOnly, InputRuleScope::EveryWrite] {
        let original = schema(scope).validate().unwrap();
        let protocol = resource_schema_to_protocol(&original).unwrap();
        let json = serde_json::to_value(&protocol).unwrap();
        assert_eq!(json["input_rules"][0]["target"]["kind"], "map_keys");
        let decoded = protocol_to_resource_schema(&serde_json::from_value(json).unwrap()).unwrap();
        assert_eq!(
            original.as_schema().input_rules(),
            decoded.as_schema().input_rules()
        );
    }
}

#[test]
fn schema_admission_rejects_map_keys_target_on_a_string() {
    let invalid = ResourceSchema::new("test.Tagged")
        .attribute(AttributeSchema::optional("tags", AttributeType::string()).build())
        .input_rule(tag_rule(InputRuleScope::EveryWrite))
        .validate()
        .unwrap_err();
    assert!(
        invalid
            .input_rule_errors()
            .any(|(_, error)| *error == carina_core::schema::InputRuleError::NonStringKeyMapPath)
    );
}

#[test]
fn diagnostic_target_and_code_remain_unique_across_scopes() {
    let invalid = schema(InputRuleScope::EveryWrite)
        .input_rule(tag_rule(InputRuleScope::CreateOnly))
        .validate()
        .unwrap_err();
    assert!(
        invalid
            .input_rule_errors()
            .any(|(_, error)| *error == carina_core::schema::InputRuleError::DuplicateRule)
    );
}

#[test]
fn map_key_targets_reject_non_string_keys_and_inherited_provider_outputs() {
    for provider_populated in [false, true] {
        let attribute = if provider_populated {
            AttributeSchema::provider_populated("tags", AttributeType::map(AttributeType::string()))
                .build()
        } else {
            AttributeSchema::optional(
                "tags",
                AttributeType::map_with_key(AttributeType::int(), AttributeType::string()),
            )
            .build()
        };
        let invalid = ResourceSchema::new("test.Tagged")
            .attribute(attribute)
            .input_rule(tag_rule(InputRuleScope::EveryWrite))
            .validate()
            .unwrap_err();
        let expected = if provider_populated {
            carina_core::schema::InputRuleError::ProviderPopulatedPath
        } else {
            carina_core::schema::InputRuleError::NonStringKeyMapPath
        };
        assert!(
            invalid
                .input_rule_errors()
                .any(|(_, error)| *error == expected)
        );
    }
}

#[tokio::test]
async fn plan_admission_checks_the_host_default_tag_merge_on_both_write_kinds() {
    let registry = registry(InputRuleScope::EveryWrite);
    let config = carina_core::parser::ProviderConfig {
        name: "mock".into(),
        attributes: Default::default(),
        default_tags: (indexmap::IndexMap::<String, _>::from([(
            "AWS:private-key".into(),
            string("secret-default-value"),
        )]))
        .into(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let mut resources = vec![desired(&registry, None).into_inner()];
    carina_core::executor::normalized::apply_desired_normalization_slice(
        &mut resources,
        &[config],
        &carina_core::provider::NoopNormalizer,
        &registry,
    )
    .await
    .unwrap();
    let resource = ResolvedResource::resolve_with_schemas(resources.remove(0), &registry).unwrap();
    for create in [false, true] {
        let mut plan = Plan::new();
        plan.add(if create {
            Effect::Create(resource.clone())
        } else {
            Effect::Update {
                from: Box::new(
                    State::existing(resource.id.clone(), HashMap::new()).with_identifier("old"),
                ),
                identifier: AppliedIdentifier::new("old").unwrap(),
                to: resource.clone(),
                changed_attributes: vec!["name".into(), "tags".into()],
            }
        });
        let rejected = fixture::finalize(plan)
            .check_inputs(&registry, &HashMap::new())
            .unwrap_err();
        assert!(
            matches!(&rejected.errors()[0].kind, PlanErrorKind::InputRule { path, code, .. }
            if path == "tags" && code == "tags.reserved_prefix")
        );
        assert!(!format!("{:?}", rejected.errors()).contains("private-key"));
    }
}

#[test]
fn fingerprint_changes_when_scope_target_or_ascii_case_mode_changes() {
    let fingerprint = |rule: InputRule| {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("test.Tagged")
                .attribute(
                    AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                        .build(),
                )
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .input_rule(rule)
                .validate()
                .unwrap(),
        );
        schemas
            .get(
                "mock",
                "test.Tagged",
                carina_core::schema::SchemaKind::Resource,
            )
            .unwrap()
            .semantic_fingerprint()
    };
    let base = fingerprint(tag_rule(InputRuleScope::EveryWrite));
    assert_ne!(base, fingerprint(tag_rule(InputRuleScope::CreateOnly)));
    let changed = |target, ignore_ascii_case| {
        InputRule::new(
            InputRuleScope::EveryWrite,
            target,
            InputRuleCode::new("tags.reserved_prefix").unwrap(),
            InputPredicate::ForbiddenPrefix {
                prefix: NonEmptyString::new("aws:").unwrap(),
                ignore_ascii_case,
            },
        )
    };
    assert_ne!(
        base,
        fingerprint(changed(
            InputRuleTarget::Value(InputPath::new("name").unwrap()),
            true
        ))
    );
    assert_ne!(
        base,
        fingerprint(changed(
            InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
            false
        ))
    );
}

#[test]
fn secret_map_keys_are_checked_without_printing_keys_or_values() {
    let registry = registry(InputRuleScope::EveryWrite);
    let mut resource = desired(&registry, Some("AWS:private")).into_inner();
    let tags = Value::Deferred(carina_core::resource::DeferredValue::Secret(
        Box::new(resource.attributes.shift_remove("tags").unwrap()).into(),
    ));
    resource.attributes.insert("tags".into(), tags);
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        ResolvedResource::resolve_with_schemas(resource, &registry).unwrap(),
    ));
    let rejected = fixture::finalize(plan)
        .check_inputs(&registry, &HashMap::new())
        .unwrap_err();
    let diagnostic = format!("{:?}", rejected.errors());
    assert!(diagnostic.contains("tags.reserved_prefix"));
    assert!(!diagnostic.contains("AWS:private"));
    assert!(!diagnostic.contains("secret-tag-value"));
}

#[tokio::test]
async fn schema_default_metadata_and_materialized_defaults_match_normalized_requests() {
    use carina_core::provider::{
        BoxFuture, ProviderNormalizer, ProviderResult, SavedAttrs, StateTransformMap,
    };
    use carina_core::resource::ProviderResource;
    struct MaterializeDefault;
    impl ProviderNormalizer for MaterializeDefault {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                for resource in resources {
                    resource
                        .attributes
                        .entry("name".into())
                        .or_insert_with(|| string("aws:default"));
                }
                Ok(())
            })
        }
        fn normalize_state<'a>(
            &'a self,
            _: &'a mut StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn hydrate_read_state<'a>(
            &'a self,
            _: &'a mut StateTransformMap,
            _: &'a SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Defaulted")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .with_default(string("aws:default"))
                    .build(),
            )
            .input_rule(InputRule::value(
                InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("name.prefix").unwrap(),
                InputPredicate::ForbiddenPrefix {
                    prefix: NonEmptyString::new("aws:").unwrap(),
                    ignore_ascii_case: false,
                },
            ))
            .validate()
            .unwrap(),
    );
    for materialized in [false, true] {
        let mut resources = vec![Resource::with_provider(
            "mock",
            "test.Defaulted",
            "example",
            None,
        )];
        let normalizer: &dyn ProviderNormalizer = if materialized {
            &MaterializeDefault
        } else {
            &carina_core::provider::NoopNormalizer
        };
        carina_core::executor::normalized::apply_desired_normalization_slice(
            &mut resources,
            &[],
            normalizer,
            &schemas,
        )
        .await
        .unwrap();
        // Attribute defaults are not inserted by the host. A default supplied
        // by a provider is present in both the plan and dispatch projection.
        assert_eq!(resources[0].attributes.contains_key("name"), materialized);
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            ResolvedResource::resolve_raw_fixture(resources.remove(0), &schemas).unwrap(),
        ));
        assert_eq!(
            fixture::finalize(plan)
                .check_inputs(&schemas, &HashMap::new())
                .is_err(),
            materialized
        );
    }
}
