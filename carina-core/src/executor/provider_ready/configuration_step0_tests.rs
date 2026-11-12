//! Directory-based pin for config values restored in compact list form.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::provider::ProviderResult;
use crate::resource::{ConcreteValue, Value};
use crate::schema::{AttributeType, TypeIdentity, ValidatedConfigSchema};

#[derive(Default)]
struct ConfigListFactory {
    string_or_list: bool,
    custom_calls: AtomicUsize,
    semantic_calls: AtomicUsize,
}

impl ProviderFactory for ConfigListFactory {
    fn name(&self) -> &str {
        "owner"
    }

    fn display_name(&self) -> &str {
        "directory config callback fixture"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        let list = AttributeType::list(AttributeType::refined_string(
            Some(TypeIdentity::new(
                Some("owner"),
                Vec::<String>::new(),
                "Endpoint",
            )),
            None,
            None,
            None,
        ));
        let endpoints = if self.string_or_list {
            AttributeType::union(vec![AttributeType::string(), list])
        } else {
            list
        };
        ValidatedConfigSchema::validate(BTreeMap::from([("endpoints".into(), endpoints)])).unwrap()
    }

    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &crate::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.semantic_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn validate_custom_type(
        &self,
        _: &TypeIdentity,
        value: &crate::provider::CanonicalCustomTypeValue,
        _: &crate::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.custom_calls.fetch_add(1, Ordering::SeqCst);
        if value.as_str() == "bad" {
            Err(crate::provider::ProviderError::invalid_input(
                "Endpoint callback rejects bad",
            ))
        } else {
            Ok(())
        }
    }

    fn create_provider(
        &self,
        _: &crate::provider::ProviderSession,
        _: crate::provider::InitializeOutcomeMint,
    ) -> crate::provider::BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>>
    {
        panic!("config rejection must precede initialization")
    }

    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
        Vec::new()
    }

    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
        crate::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}

#[test]
fn provider_config_directory_compact_string_list_checks_callback_before_factory() {
    check_callback_before_factory(false, false);
    check_callback_before_factory(true, false);
}

#[test]
fn provider_config_directory_canonical_union_list_checks_callback_before_factory() {
    check_callback_before_factory(false, true);
}

fn check_callback_before_factory(compact: bool, string_or_list: bool) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("values.crn"),
        "let Endpoint = 'bad'\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        "provider owner { endpoints = [Endpoint] }\n",
    )
    .unwrap();
    let loaded = crate::config_loader::load_configuration_with_config(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &crate::schema::SchemaRegistry::new(),
    )
    .unwrap();
    let original = &loaded.parsed.providers[0];
    let attributes = original
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
        .collect::<crate::parser::ConfigurationAttributes<Value>>();

    let factory = ConfigListFactory {
        string_or_list,
        ..ConfigListFactory::default()
    };
    let mut attributes = attributes.clone();
    if compact {
        let Value::Concrete(ConcreteValue::List(values)) = &attributes["endpoints"] else {
            panic!("sibling binding must resolve to a list")
        };
        let strings = values
            .iter()
            .map(|value| match value {
                Value::Concrete(ConcreteValue::String(value)) => value.clone(),
                _ => panic!("fixture has only strings"),
            })
            .collect();
        attributes.insert(
            "endpoints",
            Value::Concrete(ConcreteValue::StringList(strings)),
        );
    }
    let configs = crate::parser::ResolvedProviderConfigs::from_test_configs(vec![
        crate::parser::ProviderConfig {
            name: "owner".into(),
            attributes,
            default_tags: Default::default(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        },
    ])
    .unwrap();
    let result = prepare_provider_ready_config(&factory, configs.iter().next().unwrap());
    eprintln!(
        "STEP0 compact={compact} string_or_list={string_or_list} ready={} custom_calls={} semantic_calls={}",
        result.is_ok(),
        factory.custom_calls.load(Ordering::SeqCst),
        factory.semantic_calls.load(Ordering::SeqCst),
    );
    assert!(
        result.is_err(),
        "callback-rejected config reached validate_config: compact={compact}"
    );
    assert_eq!(factory.custom_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.semantic_calls.load(Ordering::SeqCst), 0);
}
