use super::*;

fn function_validation(source: &str) -> Vec<String> {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed =
        super::super::parse_file(source, Path::new("function-default.crn"), &catalog).unwrap();
    let schemas = crate::schema::SchemaRegistry::new();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    configuration
        .judged()
        .report()
        .findings()
        .iter()
        .map(|finding| finding.detail().to_string())
        .collect()
}

#[test]
fn function_default_cannot_replace_an_invalid_body() {
    let errors = function_validation("fn f(a: Int, s: String = 'x'): String { a }");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("function 'f': return value"),
        "{errors:?}"
    );
}

#[test]
fn function_default_cannot_reject_a_valid_body() {
    let errors = function_validation(r#"fn tag(env: String, n: Int = 3): String { "${env}" }"#);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn tp3_bare_handle_return_keeps_binding_diagnostic_ownership() {
    let mut schemas = crate::schema::SchemaRegistry::new();
    schemas.insert(
        "test",
        crate::schema::ResourceSchema::new("Thing")
            .attribute(
                crate::schema::AttributeSchema::optional(
                    "name",
                    crate::schema::AttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let snapshot = crate::test_utils::loaded_fixture_snapshot(schemas);
    let catalog = snapshot.provider_context().as_provider_context();
    let parsed = super::super::parse_file(
        "fn identity(r: test.Thing): String { r }",
        Path::new("handle.crn"),
        &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let raw = judge_expressions(&parsed, snapshot.schema_registry(), catalog);
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        snapshot.schema_registry(),
        catalog,
    );
    let messages = |report: &ExpressionReport| {
        report
            .findings()
            .iter()
            .map(|finding| finding.detail().to_string())
            .collect::<Vec<_>>()
    };
    assert!(
        configuration.judged().report().findings().is_empty(),
        "bare handles belong to binding validation; authored: {:?}; with promises: {:?}",
        messages(&raw),
        messages(configuration.judged().report())
    );
}

#[test]
fn conversion_syntax_equality_ignores_child_and_judgement_metadata() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for source in [
        "fn tag(env: String, n: Int = 3): String { env }",
        "mock.Sink { target = 'ok' }",
        "let value = if 42 as String { 'ok' } else { 'ok' }",
    ] {
        let left = super::super::parse_file(source, Path::new("equality.crn"), &catalog).unwrap();
        let right = super::super::parse_file(source, Path::new("equality.crn"), &catalog).unwrap();
        let left = left.parser_binding_metadata.syntax().expressions();
        let mut right = right.parser_binding_metadata.syntax().expressions().clone();
        right.report = left.report.clone();
        assert_eq!(left, &right, "{source}");
    }
}
