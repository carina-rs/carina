use super::*;
use crate::provider::*;
use crate::resource::{AbsoluteBindingAddress, ConcreteValue, NoPlaceholder};
use crate::schema::{AttributeType, DslTransform, TypeIdentity};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use crate::test_utils::configuration::{ConfigFactory as Factory, provider_directory as directory};

#[test]
fn undeclared_configuration_keys_reject_before_values_or_factory_validation() {
    let root = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        (
            "providers.crn",
            "let east = provider owner { zzz = producer.id regoin = 'ap-northeast-1' aaa = 'private-value' }",
        ),
        (
            "main.crn",
            "let producer = mock.test.resource { name = 'producer' }",
        ),
    ]);
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(root.path()).unwrap(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let attrs = &parsed.parsed().providers[0].attributes;
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([(
        "region".into(),
        AttributeType::string(),
    )]))
    .unwrap();
    let factory = Factory::new(schema.clone());
    let analysis = analyze(&factory, "owner", attrs)
        .expect_err("analysis must reject every undeclared field, even a pending one");
    assert!(analysis.diagnostics().iter().all(|diagnostic| matches!(
        diagnostic.schema_error(),
        Some(TypeError::UnknownAttribute { .. })
    )));
    let analysis = analysis.to_string();
    let complete = admitted(&factory, "owner", Some("east"), attrs, &schema)
        .expect_err("construction must reject undeclared fields before inspecting their values")
        .to_string();
    assert_eq!(complete, analysis);
    let unknowns: Vec<_> = analysis.lines().collect();
    assert_eq!(unknowns.len(), 3, "{analysis}");
    for (diagnostic, name) in unknowns.iter().zip(["zzz", "regoin", "aaa"]) {
        assert!(
            diagnostic.contains(&format!("Unknown attribute '{name}'")),
            "{analysis}"
        );
    }
    assert!(analysis.contains("did you mean 'region'?"), "{analysis}");
    assert!(!analysis.contains("private-value"));
    assert!(factory.seen.lock().unwrap().is_empty());
    assert_eq!(factory.custom_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn provider_configuration_suggestions_share_resource_tie_breaking() {
    let configs = directory("provider owner { hat = 'value' }", "let sibling = 'value'");
    let attributes = configs.iter().next().unwrap().attributes();
    for primary in [["cat", "bat"], ["bat", "cat"]] {
        let expected = TypeError::unknown_attribute("hat", &primary, &[]);
        assert_eq!(
            expected.to_string(),
            "Unknown attribute 'hat', did you mean 'bat'?"
        );
        let schema = ValidatedConfigSchema::validate(
            primary
                .into_iter()
                .map(|name| (name.into(), AttributeType::string()))
                .collect(),
        )
        .unwrap();
        let factory = Factory::new(schema.clone());
        for error in [
            analyze(&factory, "owner", attributes).unwrap_err(),
            admitted(&factory, "owner", None, attributes, &schema).unwrap_err(),
        ] {
            let diagnostics = error.diagnostics();
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(
                diagnostics[0].schema_error().unwrap().to_string(),
                expected.to_string()
            );
        }
        assert!(factory.seen.lock().unwrap().is_empty());
    }
}

#[test]
fn undeclared_configuration_keys_reject_with_an_empty_declaration() {
    let factory = Factory::new(ValidatedConfigSchema::default());
    let configs = directory("provider owner { typo = 'value' }", "let sibling = 'value'");
    let config = configs.iter().next().unwrap();
    assert!(analysis(&factory, "owner", config.attributes()).is_err());
    assert!(provider::construction(&factory, config, &ProviderPlan::state_only()).is_err());
    assert!(factory.seen.lock().unwrap().is_empty());
    assert_eq!(factory.initialized.load(Ordering::SeqCst), 0);
}

#[test]
fn analysis_and_complete_configuration_have_distinct_types() {
    let factory = Factory::new(ValidatedConfigSchema::default());
    let attrs = ConfigurationAttributes::new();
    let analyzed = analysis(&factory, "owner", &attrs).unwrap();
    let complete = admitted(&factory, "owner", None, &attrs, &factory.schema).unwrap();
    assert_ne!(
        std::any::type_name_of_val(&analyzed),
        std::any::type_name_of_val(&complete)
    );
}

#[test]
fn operational_provider_configuration_never_omits_unresolved_fields() {
    let root = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        ("providers.crn", "provider owner { value = producer.id }"),
        (
            "main.crn",
            "let producer = mock.test.resource { name = 'producer' }",
        ),
    ]);
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(root.path()).unwrap(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let attributes = &parsed.parsed().providers[0].attributes;
    for declared in [false, true] {
        let schema = if declared {
            ValidatedConfigSchema::validate(BTreeMap::from([(
                "value".into(),
                AttributeType::string(),
            )]))
            .unwrap()
        } else {
            ValidatedConfigSchema::default()
        };
        let factory = Factory::new(schema.clone());
        let error = admitted(&factory, "owner", None, attributes, &schema)
            .expect_err("operational admission must not drop an unresolved value");
        assert!(factory.seen.lock().unwrap().is_empty());
        if declared {
            let ProviderConfigPreparationError::InvalidAttribute { error, .. } = error else {
                panic!("typed attribute error")
            };
            assert!(matches!(error.errors()[0], TypeError::Knowledge { .. }));
            assert!(analyze(&factory, "owner", attributes).unwrap().is_empty());
        } else {
            assert!(matches!(
                error,
                ProviderConfigPreparationError::UnknownAttributes(_)
            ));
            assert!(analyze(&factory, "owner", attributes).is_err());
        }
    }
}

#[test]
fn provider_config_admission_owns_exact_canonical_payload() {
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([
        (
            "region".into(),
            AttributeType::enum_(
                TypeIdentity::new(Some("owner"), Vec::<String>::new(), "Region"),
                None,
                vec![],
                None,
                Some(DslTransform::HyphenToUnderscore),
            ),
        ),
        ("extension".into(), AttributeType::string()),
    ]))
    .unwrap();
    let factory = Factory::new(schema);
    let configs = directory(
        "let east = provider owner { region = Region extension = 'kept' }\n",
        "let Region = owner.Region.us_east_1\n",
    );
    let source = configs.iter().next().unwrap();
    let route = ProviderInstanceKey::new("owner", Some("east".into()));
    let construction =
        provider::construction(&factory, source, &ProviderPlan::state_only()).unwrap();
    let ready = construction.session().configuration();
    assert_eq!(ready.instance(), &route);
    assert_eq!(ready.region(), Some("us-east-1"));
    assert_eq!(
        ready.attributes()["extension"],
        Value::Concrete(ConcreteValue::String("kept".into()))
    );
    assert_eq!(ready.inner.admitted.placements().len(), 2);
    assert_eq!(
        ready.inner.admitted.placements().len(),
        ready.attributes().len()
    );
    crate::test_utils::provider_text::block_on(ProviderBoundary::initialize(&construction))
        .unwrap();
    assert_eq!(factory.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory.witnesses.load(Ordering::SeqCst),
        1,
        "initialization must retain the admitted declaration snapshot"
    );
}

#[test]
fn provider_config_receipt_rejects_other_declaration_instance_revision() {
    let factory = Factory::new(
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "value".into(),
            AttributeType::string(),
        )]))
        .unwrap(),
    );
    let declaration = ConfigurationDeclaration::provider(factory.name(), factory.schema.clone());
    let other_declaration =
        ConfigurationDeclaration::provider(factory.name(), factory.schema.clone());
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let other_catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let route = ProviderInstanceKey::new("owner", Some("east".into()));
    let key = crate::parser::AttributeKey::from("value");
    let expected = declaration
        .destination::<kind::ProviderConfigAttribute>(
            &key,
            InstanceIdentity::Provider(route.clone()),
            CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
        )
        .unwrap();
    let different = [
        (
            declaration
                .destination(
                    &key,
                    InstanceIdentity::Provider(ProviderInstanceKey::new(
                        "owner",
                        Some("west".into()),
                    )),
                    CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
                )
                .unwrap(),
            SlotMismatch::Instance,
        ),
        (
            other_declaration
                .destination(
                    &key,
                    InstanceIdentity::Provider(route.clone()),
                    CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
                )
                .unwrap(),
            SlotMismatch::Declaration,
        ),
        (
            declaration
                .destination(
                    &key,
                    InstanceIdentity::Provider(route.clone()),
                    CatalogRevision(CatalogSnapshot::Provider(
                        other_catalog.custom_types.clone(),
                    )),
                )
                .unwrap(),
            SlotMismatch::CatalogRevision,
        ),
    ];
    let value = Value::<NoPlaceholder, AbsoluteBindingAddress>::Concrete(ConcreteValue::String(
        "known".into(),
    ));
    let lookup_inputs = ProviderValidationInputs::for_schema("owner");
    let lookup = crate::parser::custom_type_lookup_for_factory(&factory, &lookup_inputs);
    let checked = crate::validation::deferred_types::TypeCheckSession::run(&catalog, |session| {
        let placed = field(&expected, &value, session, &lookup)
            .unwrap()
            .ready()
            .unwrap();
        for (wrong, mismatch) in different {
            let mut slot = TypedSlot::new(wrong);
            assert_eq!(slot.place(placed.clone()), Err(mismatch));
            assert!(slot.value().is_none());
        }
    });
    checked.publish(&mut Vec::new());
}

#[test]
fn provider_config_directory_rejects_undeclared_keys_and_preserves_optional_fields() {
    let factory = Factory::new(
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "optional".into(),
            AttributeType::bool(),
        )]))
        .unwrap(),
    );
    let configs = directory(
        "provider owner { extension = Input }\n",
        "let Input = 'opaque'\n",
    );
    let error = provider::construction(
        &factory,
        configs.iter().next().unwrap(),
        &ProviderPlan::state_only(),
    )
    .err()
    .expect("an undeclared extension cannot initialize a provider");
    assert!(error.to_string().contains("Unknown attribute 'extension'"));
    assert!(factory.seen.lock().unwrap().is_empty());
    let configs = directory("provider owner {}", "let sibling = 'value'");
    let construction = provider::construction(
        &factory,
        configs.iter().next().unwrap(),
        &ProviderPlan::state_only(),
    )
    .unwrap();
    assert!(
        construction
            .session()
            .configuration()
            .attributes()
            .is_empty()
    );
    assert!(
        !construction
            .session()
            .configuration()
            .attributes()
            .contains_key("optional")
    );
    assert_eq!(factory.seen.lock().unwrap().len(), 1);
}

#[test]
fn provider_config_directory_empty_and_named_sessions_are_admitted() {
    let factory = Factory::new(ValidatedConfigSchema::default());
    let configs = directory("let east = provider owner {}\n", "let Unused = 'sibling'\n");
    for default_route in [true, false] {
        let source = configs.iter().next().unwrap();
        let construction = if default_route {
            provider::default_construction(&factory, source, &ProviderPlan::state_only())
        } else {
            provider::construction(&factory, source, &ProviderPlan::state_only())
        }
        .unwrap();
        let route = ProviderInstanceKey::new(
            "owner",
            if default_route {
                None
            } else {
                Some("east".into())
            },
        );
        assert_eq!(construction.session().configuration().instance(), &route);
        assert!(
            construction
                .session()
                .configuration()
                .attributes()
                .is_empty()
        );
        crate::test_utils::provider_text::block_on(ProviderBoundary::initialize(&construction))
            .unwrap();
    }
    assert_eq!(factory.initialized.load(Ordering::SeqCst), 2);
}

#[test]
fn provider_config_directory_checks_full_destination_after_resolution() {
    use crate::schema::{
        FieldGroup, ListElements, PatternMatch, PatternSpec, StringIdentityLayer,
        StringMemberConstraints, StructField,
    };
    let pattern = |text: &str| PatternSpec::new(text, PatternMatch::Full);
    let member_string = AttributeType::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::new(
                Some("owner"),
                Vec::<String>::new(),
                "Label",
            )),
            Some(pattern("a.*")),
            None,
            None,
        ),
        StringMemberConstraints::new(Some(pattern(".*z")), Some((Some(2), Some(4)))),
    );
    let cases = [
        (
            "identity and member",
            member_string,
            "'abz'",
            vec!["'bbz'", "'abb'", "'abbbbbz'"],
        ),
        (
            "typed map key/value/count",
            AttributeType::refined_map(
                AttributeType::refined_string(None, Some(pattern("ok.*")), None, None),
                AttributeType::refined_int(None, Some((Some(1), Some(3)))),
                Some((Some(1), Some(1))),
            ),
            "{ ok = 2 }",
            vec!["{ bad = 2 }", "{ ok = 9 }", "{ ok = 2 okay = 3 }"],
        ),
        (
            "list cardinality/uniqueness",
            AttributeType::refined_list(
                AttributeType::string(),
                true,
                Some((Some(1), Some(2))),
                ListElements::Unique,
            ),
            "['a']",
            vec!["[]", "['a', 'a']", "['a', 'b', 'c']"],
        ),
        (
            "struct required/provider-populated",
            AttributeType::struct_(
                "Settings",
                vec![
                    StructField::required("name", AttributeType::string()).build(),
                    StructField::provider_populated("generated", AttributeType::string()).build(),
                ],
            ),
            "{ name = 'ok' }",
            vec!["{}", "{ name = 'ok' generated = 'forbidden' }"],
        ),
        (
            "struct input groups",
            AttributeType::struct_with_field_groups(
                "Choice",
                vec![
                    StructField::optional("first", AttributeType::string()).build(),
                    StructField::optional("second", AttributeType::string()).build(),
                ],
                vec![FieldGroup::exactly_one(["first", "second"]).unwrap()],
            ),
            "{ first = 'ok' }",
            vec!["{}", "{ first = 'ok' second = 'conflict' }"],
        ),
        (
            "union arms",
            AttributeType::union(vec![
                AttributeType::bool(),
                AttributeType::refined_int(None, Some((Some(1), Some(3)))),
            ]),
            "true",
            vec!["9", "'string'"],
        ),
    ];
    for (label, field_type, valid, invalid) in cases {
        for (source, accepted) in
            std::iter::once((valid, true)).chain(invalid.into_iter().map(|value| (value, false)))
        {
            let factory = Factory::new(
                ValidatedConfigSchema::validate(BTreeMap::from([(
                    "value".into(),
                    field_type.clone(),
                )]))
                .unwrap(),
            );
            let configs = directory(
                "provider owner { value = Input }",
                &format!("let Input = {source}\n"),
            );
            let result = provider::construction(
                &factory,
                configs.iter().next().unwrap(),
                &ProviderPlan::state_only(),
            );
            assert_eq!(
                result.is_ok(),
                accepted,
                "{label}, {source}: {:?}",
                result.err()
            );
            assert_eq!(
                factory.seen.lock().unwrap().len(),
                usize::from(accepted),
                "{label}, {source}"
            );
            assert_eq!(factory.initialized.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn analysis_declared_field_cannot_silently_drop_pending() {
    let factory = Factory::new(
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "value".into(),
            AttributeType::string(),
        )]))
        .unwrap(),
    );
    let declaration = ConfigurationDeclaration::provider(factory.name(), factory.schema.clone());
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let route = ProviderInstanceKey::new("owner", None);
    let key = crate::parser::AttributeKey::from("value");
    let destination = declaration
        .destination::<kind::ProviderConfigAttribute>(
            &key,
            InstanceIdentity::Provider(route.clone()),
            CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
        )
        .unwrap();
    let inputs = ProviderValidationInputs::for_schema("owner");
    let lookup = crate::parser::custom_type_lookup_for_factory(&factory, &inputs);
    let value = Value::root_resource_ref("producer", "id", vec![]);
    let pending = crate::validation::deferred_types::TypeCheckSession::run(&catalog, |session| {
        assert!(matches!(
            field(&destination, &value, session, &lookup).unwrap(),
            Admission::Pending(_)
        ));
        let mut aggregate = ConfigurationBuilder::new(Arc::clone(&declaration), route.clone());
        let error =
            Analysis::declared(&destination, &key, &value, session, &lookup, &mut aggregate)
                .expect_err("a declared-field result must not silently omit Pending");
        assert!(matches!(
            error.errors.as_slice(),
            [TypeError::Knowledge { .. }]
        ));
    });
    pending.publish(&mut Vec::new());
}

fn classification_errors() -> Vec<(KnowledgeError<AbsoluteBindingAddress>, &'static str)> {
    vec![
        (
            KnowledgeError::Cycle {
                operation: "user function",
            },
            "Validation failed: cycle while resolving user function",
        ),
        (
            KnowledgeError::InvalidStage {
                operation: "fixture classification",
            },
            "Validation failed: fixture classification is not valid at this resolution boundary",
        ),
        (
            KnowledgeError::UnavailableHistoricalSecret(
                serde_json::from_str::<crate::resource::UnavailableSecretError>(
                    r#"{"path":"saved.plan.provider.token"}"#,
                )
                .unwrap(),
            ),
            "Validation failed: secret value unavailable at saved.plan.provider.token",
        ),
    ]
}

#[test]
fn provider_classification_preserves_knowledge_category_and_text() {
    for (error, expected) in classification_errors() {
        let diagnostic = attribute_error("owner", &"value".into(), error.into());
        let ProviderConfigPreparationError::InvalidAttribute { error, .. } = diagnostic else {
            panic!("attribute error")
        };
        assert_eq!(error.errors()[0].to_string(), expected);
        assert!(matches!(
            error.errors()[0],
            TypeError::Knowledge {
                attribute: None,
                ..
            }
        ));
    }
}

#[test]
fn saved_historical_provider_stops_before_configuration_admission() {
    use crate::parser::{ProviderConfig, ResolvedProviderConfigs, SavedProviderConfigs};
    use crate::resource::{ConcreteValue, DeferredValue, SnapshotLeaf, Value};
    let token: Value = Value::Deferred(DeferredValue::Secret(
        Box::new(Value::Concrete(ConcreteValue::String(
            "fixture-secret".into(),
        )))
        .into(),
    ));
    let config = ProviderConfig::<SnapshotLeaf, crate::resource::AbsoluteBindingAddress> {
        name: "mock".into(),
        binding: None,
        attributes: [(
            "token".to_owned(),
            crate::value::redact_state_snapshot_value(&token.to_snapshot()).unwrap(),
        )]
        .into_iter()
        .collect(),
        default_tags: Default::default(),
        unresolved_attributes: Default::default(),
        source: None,
        version: None,
        revision: None,
    };
    let saved: SavedProviderConfigs =
        serde_json::from_value(serde_json::to_value(vec![config]).unwrap()).unwrap();
    let saved = saved.redact_secrets().unwrap();
    let error = ResolvedProviderConfigs::try_from(saved).unwrap_err();
    assert!(matches!(
        error,
        crate::parser::ProviderConfigInvariantError::UnavailableSecret(_)
    ));
    assert_eq!(
        error.to_string(),
        "secret value unavailable at provider mock"
    );
}

#[test]
fn provider_config_declared_fields_preserve_saved_and_protocol_wire_bytes() {
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([
        ("first".into(), AttributeType::string()),
        ("second".into(), AttributeType::bool()),
        ("token".into(), AttributeType::string()),
    ]))
    .unwrap();
    let factory = Factory::new(schema);
    let configs = directory(
        "provider owner { second = true first = 'value' token = 'test-token' }",
        "let sibling = 'value'",
    );
    let ready = provider::prepare(&factory, configs.iter().next().unwrap()).unwrap();
    let wire: Vec<_> = ready
        .wire_attributes()
        .transport_iter()
        .map(|(key, value)| (key, value.value(), value.is_secret()))
        .collect();
    let saved = crate::parser::SavedProviderConfigs::from(&configs)
        .redact_secrets()
        .unwrap();
    let actual = serde_json::json!({
        "wire": serde_json::to_string(&wire).unwrap(),
        "saved_provider_configs": serde_json::to_string(&saved).unwrap(),
    });
    let main: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/configuration_wire.json")).unwrap();
    assert_eq!(
        actual, main,
        "valid configuration wire bytes must match main 55d8904e4"
    );
    assert_eq!(
        ready.inner.admitted.placements().len(),
        ready.attributes().len()
    );
}
