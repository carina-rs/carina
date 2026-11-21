use super::*;

fn function_validation(source: &str) -> Vec<String> {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed =
        super::super::parse_file(source, Path::new("function-default.crn"), &catalog).unwrap();
    let mut schemas = crate::schema::SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Source")
            .attribute(
                crate::schema::AttributeSchema::provider_populated(
                    "count",
                    crate::schema::AttributeType::int(),
                )
                .build(),
            )
            .attribute(
                crate::schema::AttributeSchema::provider_populated(
                    "name",
                    crate::schema::AttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
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
    let errors = function_validation(
        "fn f(a: Int, s: String = 'x'): String { a }\nlet src = mock.Source {}\nlet result = f(src.count)",
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("function 'f': return value"),
        "{errors:?}"
    );
}

#[test]
fn function_default_cannot_reject_a_valid_body() {
    let errors = function_validation(
        "fn tag(env: String, n: Int = 3): String { \"${env}\" }\nlet src = mock.Source {}\nlet result = tag(src.name)",
    );
    assert!(errors.is_empty(), "{errors:?}");
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
