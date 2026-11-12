use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use tower_lsp::lsp_types::{DiagnosticSeverity, Position};

#[test]
fn conversion_lsp_chains_report_one_outer_diagnostic() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    for depth in [2, 3] {
        let body = format!("mock.Sink {{ name = 'ok'{} }}", " as String".repeat(depth));
        let source = format!("provider mock {{}}\n{body}\n");
        let diagnostics = engine.analyze(&create_document(&source), None);
        let errors = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            errors[0].range.start,
            Position::new(1, body.find("'ok'").unwrap() as u32)
        );
        assert_eq!(
            errors[0].range.end,
            Position::new(1, (body.rfind("String").unwrap() + 6) as u32)
        );
        assert_eq!(
            errors[0].message,
            "a conversion cannot be the operand of another conversion"
        );
    }
}

#[test]
fn conversion_lsp_parse_rejection_keeps_the_convert_range_and_cause() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let source = "let converted = if 42 as String { 'ok' } else { 'ok' }\n";
    let path = directory.path().join("sibling.crn");
    std::fs::write(&path, source).unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let sink = mock.Sink { name = converted }\n",
    )
    .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let document = create_document_at(source, path.clone());
    let diagnostics = engine.analyze(&document, Some(&path));
    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{diagnostics:#?}");
    assert_eq!(errors[0].range.start, Position::new(0, 19));
    assert!(
        errors[0]
            .message
            .starts_with("conversion is not supported inside this expression")
    );
    let parsed = carina_core::parser::parse_file(
        source,
        &path,
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap_err();
    let conversion = carina_core::ConversionError::from_error(&parsed).unwrap();
    assert_eq!(errors[0].message, conversion.diagnostic_message());
}

#[test]
fn conversion_lsp_directory_alias_rejection_is_reported_once_at_convert() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let source = "let converted = 'ok' as String\n";
    let path = directory.path().join("sibling.crn");
    std::fs::write(&path, source).unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let one = mock.Sink { name = converted }\nlet two = mock.Sink { name = converted }\n",
    )
    .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let document = create_document_at(source, path.clone());
    let diagnostics = engine.analyze(&document, Some(&path));
    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{diagnostics:#?}");
    assert_eq!(errors[0].range.start, Position::new(0, 16));
    assert_eq!(
        errors[0].message,
        "conversion target must be a loaded named scalar type"
    );
}

#[test]
fn conversion_sibling_coverage_preserves_ordinary_provider_callback_counts() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let source = "let independent = 'ok'\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    std::fs::write(
        directory.path().join("sibling.crn"),
        "let sink = mock.Sink { name = 'ok' }\n",
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "name",
                    AttributeType::refined_string_with_validator(
                        carina_core::schema::TypeIdentity::from_dotted("mock.Name"),
                        None,
                        None,
                        carina_core::schema::validator(move |_| {
                            counter.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }),
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let document = create_document_at(source, path.clone());
    engine.analyze(&document, Some(&path));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "main's anonymous-identity normalization calls once; conversion coverage adds none"
    );
}
