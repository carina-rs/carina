use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use tower_lsp::lsp_types::{DiagnosticSeverity, Position};

#[test]
fn conversion_module_output_and_its_sibling_share_admission_coverage() {
    use carina_core::schema::TypeIdentity;
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "name",
                    AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("mock.Name")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let engine = custom_engine(schemas);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "arguments { value: String }\nattributes { out: String = value }\n",
    )
    .unwrap();
    let path = directory.path().join("main.crn");
    for (attributes, rejected) in [
        ("plain = net.out name = 'ok' as mock.Name", false),
        ("name = net.out as mock.Name", false),
        ("plain = net.out as mock.Name", true),
    ] {
        let source = format!(
            "let child = use {{ source = './child' }}\nlet net = child {{ value = 'ok' }}\nmock.Sink {{ {attributes} }}\n"
        );
        std::fs::write(&path, &source).unwrap();
        let diagnostics = engine.analyze(&create_document_at(&source, path.clone()), Some(&path));
        let errors = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect::<Vec<_>>();
        assert_eq!(
            errors.len(),
            usize::from(rejected),
            "{source}\n{diagnostics:#?}"
        );
        if rejected {
            assert!(
                errors[0].message.contains("matching type identity"),
                "{errors:#?}"
            );
        }
    }
}

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
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas.clone()), vec!["mock".into()]);
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
            .starts_with("conversion is not supported at this position")
    );
    let parsed = carina_core::parser::parse_file(
        source,
        &path,
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let catalog =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let configuration =
        carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let presented = carina_core::validation::ConversionValidation::<std::convert::Infallible>::new(
        configuration.judged().report(),
    )
    .complete()
    .into_presented();
    let conversions = presented
        .iter()
        .filter_map(|finding| match finding {
            carina_core::validation::PresentedFinding::Conversion(finding) => Some(finding),
            carina_core::validation::PresentedFinding::Ordinary(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(conversions.len(), 1);
    assert_eq!(errors[0].message, conversions[0].message());
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

#[test]
fn conversion_report_fallback_preserves_ordinary_errors_and_detects_missing_outcomes() {
    use carina_core::validation::admission::attributes::{
        ResourceAdmissionRequest, ResourceAttributeInputs,
    };
    use carina_core::{binding_index::BindingIndex, validation::ConversionValidation};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    for file in ["main.crn", "sibling.crn"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(file);
        let mut ordinary = Vec::new();
        for converted in [false, true] {
            let expression = if converted { "'ok' as String" } else { "'ok'" };
            let source = format!("mock.Sink {{ name = {expression} plain = 42 }}");
            std::fs::write(&path, &source).unwrap();
            if file == "sibling.crn" {
                std::fs::write(directory.path().join("main.crn"), "let main = 'ok'").unwrap();
            }
            let parsed = carina_core::parser::parse_file(&source, &path, &catalog).unwrap();
            let configuration =
                carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
            let judged = configuration.judged();
            let tree = carina_core::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
            let resource = tree.resources().next().unwrap();
            let values = resource.resolved_attributes();
            let inputs = ResourceAttributeInputs::from_report(resource, &values, judged.report());
            let classified = inputs.classify(
                &carina_core::resource::knowledge::KnowledgeContext::validation(),
                |_, _| {},
            );
            let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
            let mut validation: ConversionValidation = ConversionValidation::new(judged.report());
            carina_core::validation::deferred_types::with_session_for_test(&catalog, |session| {
                let findings = validation.admit_resource(ResourceAdmissionRequest {
                    resource,
                    parsed: judged.file(),
                    schema: resource.schema(&schemas).unwrap(),
                    attributes: &classified,
                    quoted: resource.quoted_string_attrs(),
                    inputs: &inputs,
                    bindings: &bindings,
                    session,
                    schemas: &schemas,
                });
                ordinary.push(
                    findings
                        .errors()
                        .chain(findings.symbolic_errors().map(|(_, _, error)| error))
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                );
                if file == "sibling.crn" {
                    // Missing owner in the unrendered report exercises the same
                    // fallback as a sibling omitted from the buffer's tree.
                    let empty =
                        carina_core::parser::parse_file("let other = 'ok'", &path, &catalog)
                            .unwrap();
                    let empty_configuration = carina_core::config_loader::load_buffer_configuration(
                        &empty, &schemas, &catalog,
                    );
                    validation.admit_remaining_resources(
                        &tree,
                        empty_configuration.judged(),
                        &bindings,
                        session,
                    );
                } else {
                    validation.admit_remaining_resources(&tree, judged, &bindings, session);
                }
            });
            let presented = validation.complete().into_presented();
            let conversions = presented
                .iter()
                .filter_map(|finding| match finding {
                    carina_core::validation::PresentedFinding::Conversion(finding) => Some(finding),
                    carina_core::validation::PresentedFinding::Ordinary(_) => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(conversions.len(), usize::from(converted), "{presented:?}");
            assert_eq!(presented.len(), usize::from(converted), "{presented:?}");
            assert!(
                !carina_core::test_utils::render_presented_for_test(&presented)
                    .contains("bug in carina")
            );
        }
        assert_eq!(ordinary[0], ordinary[1], "{ordinary:#?}");
        assert!(!ordinary[0].is_empty());
    }
}
