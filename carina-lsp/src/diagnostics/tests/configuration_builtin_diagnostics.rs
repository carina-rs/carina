use super::*;

fn assert_builtin_diagnostics(case: &str) {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configuration_error_parity")
        .join(case);
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(directory.join("expected.json")).unwrap())
            .unwrap();
    let path = directory.join(expected["file"].as_str().unwrap());
    let source = std::fs::read_to_string(&path).unwrap();
    let factory = carina_provider_mock::ConfiguredBuiltinMock::new(
        Default::default(),
        carina_provider_mock::MockScope::memory(),
    );
    let engine = if case == "provider" {
        let config_factory = carina_core::test_utils::configuration::ConfigFactory::new(
            carina_core::schema::ValidatedConfigSchema::validate(std::collections::BTreeMap::from(
                [(
                    "target".into(),
                    carina_core::schema::AttributeType::string(),
                )],
            ))
            .unwrap(),
        )
        .named("mock");
        let mut schemas = SchemaRegistry::new();
        schemas
            .insert_provider_witness("mock", &factory.schema_witness())
            .unwrap();
        DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()])
            .with_configuration_factory(Box::new(config_factory))
    } else {
        DiagnosticEngine::new(
            loaded_providers_from_factories(vec![Box::new(factory)]),
            vec!["mock".into()],
        )
    };
    let errors: Vec<_> = engine
        .analyze(&create_document_at(&source, path.clone()), Some(&path))
        .into_iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .map(|d| serde_json::json!({"range":d.range,"message":d.message}))
        .collect();
    assert_eq!(serde_json::json!(errors), expected["errors"], "{case}");
}
#[test]
fn provider_builtin_failure_matches_diagnostic_snapshot() {
    assert_builtin_diagnostics("provider");
}
#[test]
fn let_builtin_failure_matches_diagnostic_snapshot() {
    assert_builtin_diagnostics("let");
}
#[test]
fn resource_builtin_failure_matches_diagnostic_snapshot() {
    assert_builtin_diagnostics("resource");
}
#[test]
fn backend_builtin_failure_matches_diagnostic_snapshot() {
    assert_builtin_diagnostics("backend");
}
#[test]
fn backend_builtin_and_let_failures_match_diagnostic_snapshot() {
    assert_builtin_diagnostics("backend_and_let");
}
