use super::*;
use carina_core::provider::ProviderError;
use carina_core::schema::{AttributeType, TypeIdentity, ValidatedConfigSchema};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use carina_core::test_utils::configuration::{ConfigFactory, ConfigurationDirectory};

#[test]
fn provider_config_directory_compact_callback_parity() {
    for (value, rejected) in [("bad", true), ("good", false)] {
        let source = "provider owner { endpoints = [Endpoint] }\n";
        let values = format!("let Endpoint = '{value}'\n");
        let directory =
            ConfigurationDirectory::new(&[("providers.crn", source), ("values.crn", &values)]);
        let path = directory.path().join("providers.crn");
        let schema = ValidatedConfigSchema::validate(BTreeMap::from([(
            "endpoints".into(),
            AttributeType::union(vec![
                AttributeType::string(),
                AttributeType::list(AttributeType::refined_string(
                    Some(TypeIdentity::new(
                        Some("owner"),
                        Vec::<String>::new(),
                        "Endpoint",
                    )),
                    None,
                    None,
                    None,
                )),
            ]),
        )]))
        .unwrap();
        let factory = ConfigFactory::new(schema).with_custom_validation(|_, value, _| {
            if value.as_str() == "bad" {
                Err(ProviderError::invalid_input(
                    "Endpoint callback rejects bad",
                ))
            } else {
                Ok(())
            }
        });
        let semantic = Arc::clone(&factory.semantic_calls);
        let engine = DiagnosticEngine::new(
            loaded_providers_from_factories(vec![Box::new(factory)]),
            vec!["owner".into()],
        );
        let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
        let found = diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Endpoint callback rejects bad"));
        assert_eq!(found, rejected, "{value}: {diagnostics:?}");
        if rejected {
            assert_eq!(semantic.load(Ordering::SeqCst), 0);
        } else {
            assert!(semantic.load(Ordering::SeqCst) > 0);
        }
    }
}

#[test]
fn review_r1_deferred_backend_directory_diagnostics_match_main() {
    let factory = carina_provider_mock::ConfiguredBuiltinMock::new(
        Default::default(),
        carina_provider_mock::MockScope::memory(),
    );
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(factory)]),
        vec!["mock".into()],
    );
    for case in ["resource_reference", "sibling_reference", "sibling_literal"] {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../carina-cli/tests/fixtures/backend_deferred_config")
            .join(case);
        let path = directory.join("backend.crn");
        let source = std::fs::read_to_string(&path).unwrap();
        let diagnostics = engine.analyze(&create_document_at(&source, path.clone()), Some(&path));
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .map(|d| d.message.as_str())
            .collect();
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("expected.json")).unwrap(),
        )
        .unwrap();
        let message = expected["stderr"]
            .as_str()
            .unwrap()
            .trim()
            .strip_prefix("Error: ")
            .unwrap_or("");
        if message.is_empty() {
            assert!(errors.is_empty(), "{case}: {errors:?}");
        } else {
            assert_eq!(errors, [message], "{case}");
        }
    }
}
