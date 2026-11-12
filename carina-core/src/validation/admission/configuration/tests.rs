use super::*;
use crate::parser::{ProviderConfig, ResolvedProviderConfigs};
use crate::provider::*;
use crate::resource::{AbsoluteBindingAddress, ConcreteValue, NoPlaceholder};
use crate::schema::{AttributeType, DslTransform, TypeIdentity, ValidatedProviderSchemaWitness};
use std::collections::BTreeMap;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Factory {
    schema: ValidatedConfigSchema,
    seen: Mutex<Vec<ConfigurationAttributes<Value>>>,
    witnesses: AtomicUsize,
    initialized: AtomicUsize,
}

impl Factory {
    fn new(schema: ValidatedConfigSchema) -> Self {
        Self {
            schema,
            seen: Default::default(),
            witnesses: AtomicUsize::new(0),
            initialized: AtomicUsize::new(0),
        }
    }
}

impl ProviderFactory for Factory {
    fn name(&self) -> &str {
        "owner"
    }
    fn display_name(&self) -> &str {
        "owner"
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        self.schema.clone()
    }
    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.seen
            .lock()
            .unwrap()
            .push(attributes.attributes().clone());
        Ok(())
    }
    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
        vec![]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        self.witnesses.fetch_add(1, Ordering::SeqCst);
        ValidatedProviderSchemaWitness::new(vec![], self.schema.clone(), Default::default(), vec![])
            .unwrap()
    }
    fn create_provider(
        &self,
        session: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        assert_eq!(
            self.seen.lock().unwrap().last().unwrap(),
            session.configuration().attributes()
        );
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(InitializeOutcome::unavailable()
                .with_provider(crate::test_utils::provider_text::idle_provider()))
        })
    }
}

fn directory(providers: &str, values: &str) -> ResolvedProviderConfigs {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), providers).unwrap();
    std::fs::write(directory.path().join("values.crn"), values).unwrap();
    let loaded = crate::config_loader::load_configuration_with_config(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &crate::schema::SchemaRegistry::new(),
    )
    .unwrap();
    ResolvedProviderConfigs::from_test_configs(
        loaded
            .parsed
            .providers
            .iter()
            .map(
                |config| ProviderConfig::<NoPlaceholder, AbsoluteBindingAddress> {
                    name: config.name.clone(),
                    binding: config.binding.clone(),
                    attributes: config
                        .attributes
                        .iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                value
                                    .to_expanded()
                                    .unwrap()
                                    .try_into_address_free_absolute()
                                    .unwrap(),
                            )
                        })
                        .collect(),
                    default_tags: Default::default(),
                    source: None,
                    version: None,
                    revision: None,
                    unresolved_attributes: Default::default(),
                },
            )
            .collect(),
    )
    .unwrap()
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
        provider::construction(&factory, source, &route, &ProviderPlan::state_only()).unwrap();
    let ready = construction.session().configuration();
    assert_eq!(ready.instance(), &route);
    assert_eq!(ready.region(), Some("us-east-1"));
    assert_eq!(
        ready.attributes()["extension"],
        Value::Concrete(ConcreteValue::String("kept".into()))
    );
    assert_eq!(ready.inner.canonical.admitted.fields.len(), 1);
    let mut replacement = ready.attributes().clone();
    replacement.insert("extension", Value::Concrete(ConcreteValue::Bool(false)));
    assert_ne!(&replacement, ready.attributes());
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
        .destination::<kind::ProviderConfigAttribute>(&key, &route, &catalog)
        .unwrap();
    let different = [
        (
            declaration
                .destination(
                    &key,
                    &ProviderInstanceKey::new("owner", Some("west".into())),
                    &catalog,
                )
                .unwrap(),
            SlotMismatch::Instance,
        ),
        (
            other_declaration
                .destination(&key, &route, &catalog)
                .unwrap(),
            SlotMismatch::Declaration,
        ),
        (
            declaration
                .destination(&key, &route, &other_catalog)
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
        let placed = field(&expected, &value, session, &lookup, true)
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
        &ProviderInstanceKey::new("owner", None),
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
    for route in [
        ProviderInstanceKey::new("owner", None),
        ProviderInstanceKey::new("owner", Some("east".into())),
    ] {
        let construction = provider::construction(
            &factory,
            configs.iter().next().unwrap(),
            &route,
            &ProviderPlan::state_only(),
        )
        .unwrap();
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
                &ProviderInstanceKey::new("owner", None),
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
