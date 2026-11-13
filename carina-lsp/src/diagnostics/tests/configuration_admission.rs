use super::*;
use carina_core::provider::ProviderError;
use carina_core::schema::{AttributeType, TypeIdentity, ValidatedConfigSchema};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use carina_core::test_utils::configuration::{ConfigFactory, ConfigurationDirectory};

#[test]
fn backend_attribute_error_locations_match_cli_directory_fixtures() {
    let factory = carina_provider_mock::ConfiguredBuiltinMock::new(
        Default::default(),
        carina_provider_mock::MockScope::memory(),
    );
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(factory)]),
        vec!["mock".into()],
    );
    for (case, messages) in [
        (
            "multiple",
            vec![
                "Invalid attribute 'bucket' for backend type 's3': Type mismatch: expected String, got Int",
                "Invalid attribute 'key' for backend type 's3': Type mismatch: expected String, got Int",
            ],
        ),
        (
            "single",
            vec![
                "Invalid attribute 'bucket' for backend type 's3': Type mismatch: expected String, got Int",
            ],
        ),
    ] {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../carina-cli/tests/fixtures/backend_error_locations")
            .join(case);
        for file in ["backend.crn", "main.crn", "providers.crn"] {
            let path = directory.join(file);
            let source = std::fs::read_to_string(&path).unwrap();
            let errors: Vec<_> = engine
                .analyze(&create_document_at(&source, path.clone()), Some(&path))
                .into_iter()
                .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
                .map(|diagnostic| (diagnostic.message, diagnostic.range))
                .collect();
            let expected: Vec<_> = if file == "backend.crn" {
                messages
                    .iter()
                    .enumerate()
                    .map(|(index, message)| {
                        (
                            message.to_string(),
                            Range::new(
                                Position::new(index as u32 + 1, 2),
                                Position::new(index as u32 + 1, if index == 0 { 8 } else { 5 }),
                            ),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            assert_eq!(errors, expected, "{case}/{file}");
        }
    }
}

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
fn deferred_backend_directory_diagnostics_match_validation() {
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
            .strip_prefix("Error: $DIR/backend.crn:2:3: ")
            .unwrap_or("");
        if message.is_empty() {
            assert!(errors.is_empty(), "{case}: {errors:?}");
        } else {
            assert_eq!(errors, [message], "{case}");
        }
    }
}
