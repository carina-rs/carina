//! Directory-based pin for config values restored in compact list form.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use super::*;
use crate::resource::{ConcreteValue, Value};
use crate::schema::{AttributeType, TypeIdentity, ValidatedConfigSchema};

use crate::test_utils::configuration::{ConfigFactory, provider_directory};

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
    let configs = provider_directory(
        "provider owner { endpoints = [Endpoint] }\n",
        "let Endpoint = 'bad'\n",
    );
    let mut attributes = configs.iter().next().unwrap().attributes().clone();
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
    let ty = if string_or_list {
        AttributeType::union(vec![AttributeType::string(), list])
    } else {
        list
    };
    let factory = ConfigFactory::new(
        ValidatedConfigSchema::validate(BTreeMap::from([("endpoints".into(), ty)])).unwrap(),
    )
    .with_custom_validation(|_, value, _| {
        if value.as_str() == "bad" {
            Err(crate::provider::ProviderError::invalid_input(
                "Endpoint callback rejects bad",
            ))
        } else {
            Ok(())
        }
    });
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
    assert!(
        result.is_err(),
        "callback-rejected config reached validate_config: compact={compact}"
    );
    assert_eq!(factory.custom_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.semantic_calls.load(Ordering::SeqCst), 0);
}
