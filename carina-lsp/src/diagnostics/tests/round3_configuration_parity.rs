use super::*;

fn assert_main_lsp(case: &str) {
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
    let engine = DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(factory)]),
        vec!["mock".into()],
    );
    let errors: Vec<_> = engine
        .analyze(&create_document_at(&source, path.clone()), Some(&path))
        .into_iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .map(|d| serde_json::json!({"range":d.range,"message":d.message}))
        .collect();
    assert_eq!(serde_json::json!(errors), expected["errors"], "{case}");
}
#[test]
fn round3_provider_builtin_matches_main_lsp() {
    assert_main_lsp("provider");
}
#[test]
fn round3_let_builtin_matches_main_lsp() {
    assert_main_lsp("let");
}
#[test]
fn round3_resource_builtin_matches_main_lsp() {
    assert_main_lsp("resource");
}
#[test]
fn round3_backend_builtin_matches_main_lsp() {
    assert_main_lsp("backend");
}
#[test]
fn round3_backend_builtin_and_let_match_main_lsp() {
    assert_main_lsp("backend_and_let");
}
