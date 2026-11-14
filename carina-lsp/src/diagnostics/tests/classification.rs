use super::*;
use carina_core::resource::knowledge::KnowledgeError;
use carina_core::validation::admission::attributes::ClassificationDiagnostics;

#[cfg(feature = "test-utils")]
#[test]
fn resolved_input_fallback_counter_only_records_swallowed_errors() {
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    let previous = carina_core::parser::parse_file(
        "mock.Previous { input = 1 }",
        Path::new("buffer.crn"),
        &catalog,
    )
    .unwrap();
    let sealed =
        carina_core::config_loader::load_buffer_configuration(&previous, &schemas, &catalog);
    let current = carina_core::parser::parse_file(
        "mock.Test { input = 'bad' }",
        Path::new("buffer.crn"),
        &catalog,
    )
    .unwrap();
    let tree = carina_core::parser::AnalysisTree::try_from_file(&current).unwrap();
    let resource = tree.resources().next().unwrap();
    let map = authored_validation_resources(&tree);
    let values = resource.attributes().into_owned();
    let (_, coverage) = crate::admission_coverage::with_admission_coverage(|| {
        select_admission_inputs(resource, Some(&map), &values, sealed.judged().report())
    });
    assert_eq!(coverage.resolved_inputs_fell_back, 1);
    let (_, coverage) = crate::admission_coverage::with_admission_coverage(|| {
        select_admission_inputs(resource, None, &values, sealed.judged().report())
    });
    assert_eq!(coverage.resolved_inputs_fell_back, 0);
}

#[test]
fn classification_records_keep_both_pass_order_and_duplicates_before_value_findings() {
    let source = "mock.Test { input = 'value' }";
    let path = Path::new("classification.crn");
    let document = create_document_at(source, path.into());
    let doc = document.for_diagnostics().unwrap();
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let parsed = carina_core::parser::parse_file(source, path, &catalog).unwrap();
    let tree = carina_core::parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let resource = tree.resources().next().unwrap();
    let first = KnowledgeError::InvalidStage {
        operation: "initial pass",
    };
    let second = KnowledgeError::InvalidStage {
        operation: "changed pass",
    };
    let expected = [
        first.to_string(),
        second.to_string(),
        first.to_string(),
        "value finding".into(),
    ];
    let records = ClassificationDiagnostics(vec![
        ("input".into(), first.clone()),
        ("input".into(), second),
        ("input".into(), first),
    ]);
    let mut diagnostics = render_classification_diagnostics(&doc, resource, records);
    diagnostics.push(Diagnostic {
        message: "value finding".into(),
        ..Default::default()
    });
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    for diagnostic in &diagnostics[..3] {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("carina"));
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 20), Position::new(0, 27))
        );
    }
}
