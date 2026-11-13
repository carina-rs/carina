use super::*;
use crate::provider::*;
use crate::resource::{AbsoluteBindingAddress, ConcreteValue, NoPlaceholder};
use crate::schema::{AttributeType, DslTransform, TypeIdentity};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use crate::test_utils::configuration::{ConfigFactory as Factory, provider_directory as directory};

#[test]
fn review_r1_analysis_and_complete_configuration_have_distinct_types() {
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
        let ProviderConfigPreparationError::InvalidAttribute { error, .. } = error else {
            panic!("typed attribute error")
        };
        assert!(matches!(error.errors()[0], TypeError::Knowledge { .. }));
        assert!(factory.seen.lock().unwrap().is_empty());
        assert!(analyze(&factory, "owner", attributes).unwrap().is_empty());
    }
}

#[test]
fn provider_config_admission_owns_exact_canonical_payload() {
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([(
        "region".into(),
        AttributeType::enum_(
            TypeIdentity::new(Some("owner"), Vec::<String>::new(), "Region"),
            None,
            vec![],
            None,
            Some(DslTransform::HyphenToUnderscore),
        ),
    )]))
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
    assert_eq!(ready.inner.admitted.placements().len(), 1);
    assert_eq!(ready.inner.admitted.opaque_passthrough().len(), 1);
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
fn provider_config_directory_preserves_opaque_keys_and_optional_fields() {
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
    let construction = provider::construction(
        &factory,
        configs.iter().next().unwrap(),
        &ProviderPlan::state_only(),
    )
    .unwrap();
    assert_eq!(construction.session().configuration().attributes().len(), 1);
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
                "provider owner { value = Input extension = 'opaque' }",
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
fn review_r2_analysis_declared_field_cannot_silently_drop_pending() {
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
            error.into_resolution_error(),
            ResolutionError::Knowledge(_)
        ));
    });
    pending.publish(&mut Vec::new());
}
