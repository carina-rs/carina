use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use carina_core::validation::admission::attributes::ResourceAdmissionRequest;

fn seam_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    for kind in ["Test", "Previous"] {
        schemas.insert(
            "mock",
            ResourceSchema::new(kind)
                .attribute(AttributeSchema::optional("input", AttributeType::int()).build())
                .validate()
                .unwrap(),
        );
    }
    schemas
}

fn check_report_selection(reported: bool) {
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = seam_schemas();
    let parsed = carina_core::parser::parse_file(
        "mock.Test { input = 'bad' }",
        Path::new("test.crn"),
        &config,
    )
    .unwrap();
    let previous = carina_core::parser::parse_file(
        "mock.Previous { input = 1 }",
        Path::new("test.crn"),
        &config,
    )
    .unwrap();
    let configuration =
        carina_core::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &config);
    let other =
        carina_core::test_utils::load_canonical_buffer_configuration(&previous, &schemas, &config);
    let tree = configuration.analysis_tree().unwrap();
    let resource = tree.resources().next().unwrap();
    let values = resource.values();
    let report = if reported {
        other.report()
    } else {
        configuration.report()
    };
    let inputs = select_admission_inputs(
        resource,
        &values,
        report,
        carina_core::validation::admission::attributes::SlotPrechecks::Unreported,
    );
    if reported {
        let missing = inputs.authored_evidence().unwrap_err();
        assert_eq!(missing.resource_id(), resource.id());
        assert_eq!(missing.slots().len(), 1);
        assert_eq!(missing.slots()[0].attribute(), "input");
    } else {
        assert!(inputs.authored_evidence().is_ok());
    }
    let bindings = configuration.parsed().binding_index(&config);
    let admit = |inputs| {
        let mut coverage = carina_core::validation::admission::WalkCoverage::default();
        let errors =
            carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
                let request = ResourceAdmissionRequest::new(
                    resource,
                    resource.quoted_string_attrs(),
                    inputs,
                    ValidationEvaluation::None,
                    &bindings,
                    session,
                )
                .unwrap();
                let outcome = carina_core::test_utils::admit_resource_with_coverage_for_test(
                    request,
                    &mut coverage,
                );
                assert!(outcome.classification_diagnostics.0.is_empty());
                outcome
                    .values
                    .errors()
                    .chain(
                        outcome
                            .values
                            .symbolic_errors()
                            .map(|(_, _, diagnostic)| diagnostic.error()),
                    )
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            });
        (errors, coverage)
    };
    let (errors, coverage) = admit(inputs);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("expected Int, got String"), "{errors:?}");
    if reported {
        let (expected, expected_coverage) = admit(ResourceAttributeInputs::from_report(
            resource, &values, report,
        ));
        assert_eq!(
            errors, expected,
            "selection fallback must equal direct report admission"
        );
        assert!(
            coverage.is_empty(),
            "report-only checks confer no authored coverage"
        );
        assert!(expected_coverage.is_empty());
        for report in [report, configuration.report()] {
            assert!(
                report
                    .completeness_report(&coverage)
                    .unreached_slots()
                    .any(|slot| {
                        slot.resource_id() == resource.id() && slot.attribute() == "input"
                    }),
                "fallback must leave the input slot explicitly unreached"
            );
        }
    }
}

#[test]
fn canonical_lsp_inputs_use_the_owner_report() {
    check_report_selection(false);
}

#[test]
fn canonical_lsp_report_fallback_retains_the_owner_slots() {
    check_report_selection(true);
}

#[test]
fn round5_source_resource_missing_from_report_is_explicitly_unreached() {
    check_report_selection(true);
}

#[test]
fn round2_lsp_missing_expanded_report_selects_main_fallback() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut actual = Vec::<Option<Vec<String>>>::new();
    for (argument_type, expression, target) in [
        (
            "map(map(Int))",
            "input['nested']['count']",
            AttributeType::int(),
        ),
        ("Int", "input ?? 1", AttributeType::int()),
        ("Int", "input + 1", AttributeType::int()),
        ("String", "\"${input}\"", AttributeType::string()),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("main.crn"), format!("arguments {{ input: {argument_type} }}\nprovider mock {{}}\nlet child = use {{ source = './child' }}\nlet first = child {{ input = input }}\n")).unwrap();
        std::fs::write(
            directory.path().join("child/main.crn"),
            format!(
                "arguments {{ input: {argument_type} }}\nmock.Test {{ input = {expression} }}\n"
            ),
        )
        .unwrap();
        let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Test")
                .attribute(AttributeSchema::optional("input", target).build())
                .validate()
                .unwrap(),
        );
        let path = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
        let parsed = carina_core::config_loader::parse_directory(&path, &config).unwrap();
        carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
            let graph = carina_core::module_resolver::resolve_modules_with_providerless_config(
                parsed, &path, session, &schemas,
            )
            .unwrap();
            let configuration = carina_core::test_utils::load_canonical_buffer_configuration(
                &graph, &schemas, &config,
            );
            let mut instances = graph.clone();
            for resource in &mut instances.resources {
                // allow: direct - emulate an expanded instance absent from the original report
                resource.id = carina_core::resource::ResourceId::with_provider_identity(
                    "mock",
                    "Test",
                    "unreported",
                    None,
                );
            }
            let instance_configuration =
                carina_core::test_utils::load_canonical_buffer_configuration(
                    &instances, &schemas, &config,
                );
            let tree = instance_configuration.analysis_tree().unwrap();
            let mut inputs = ExpandedDiagnosticValidationInputs {
                configuration: configuration.judged(),
                resources: tree.resources().collect(),
                binding_index: configuration.judged().parsed().binding_index(&config),
            };
            let resource = tree.resources().next().unwrap();
            let result = inputs.resource_admissions(resource.id(), session).unwrap();
            actual.push(result.map(|report| {
                report
                    .errors()
                    .chain(
                        report
                            .symbolic_errors()
                            .map(|(_, _, diagnostic)| diagnostic.error()),
                    )
                    .map(ToString::to_string)
                    .collect()
            }));
        });
    }
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("seam_round2_main.json")).unwrap();
    let actual = serde_json::json!(actual);
    assert_eq!(actual, expected["no_expanded_report"]);
}

#[test]
fn round6_report_fallback_records_the_matched_authored_slots() {
    check_report_selection(true);
}

#[test]
fn canonical_lsp_caller_values_remain_open_but_keys_are_witnessed() {
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = seam_schemas();
    let parsed =
        carina_core::parser::parse_file("mock.Test { input = 1 }", Path::new("test.crn"), &config)
            .unwrap();
    let configuration =
        carina_core::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &config);
    let tree = configuration.analysis_tree().unwrap();
    let owner = tree.resources().next().unwrap();
    let values = owner
        .with_values(&IndexMap::from([(
            "input".into(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]))
        .unwrap();
    let inputs = ResourceAttributeInputs::from_report(owner, &values, configuration.report());
    let bindings = configuration.parsed().binding_index(&config);
    carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
        let request = ResourceAdmissionRequest::new(
            owner,
            owner.quoted_string_attrs(),
            inputs,
            ValidationEvaluation::None,
            &bindings,
            session,
        )
        .unwrap();
        let mut validation =
            carina_core::validation::ConversionValidation::<OrdinaryDiagnostic>::new(
                configuration.report(),
            );
        let outcome = validation.admit_resource(request);
        assert!(
            outcome
                .values
                .errors()
                .any(|error| error.to_string().contains("expected Int, got String"))
        );
    });
}

#[test]
fn expanded_admission_missing_evidence_is_an_owner_diagnostic() {
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = seam_schemas();
    let path = std::path::PathBuf::from("/project/main.crn");
    let source = "mock.Test { input = 1 }\nmock.Previous { input = 2 }";
    let parsed = carina_core::parser::parse_file(source, &path, &config).unwrap();
    let configuration =
        carina_core::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &config);
    let tree = configuration.analysis_tree().unwrap();
    let mut resources = tree.resources();
    let owner = resources.next().unwrap();
    let other = resources.next().unwrap();
    let bindings = configuration.parsed().binding_index(&config);
    let doc = create_document_at(source, path);
    carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
        // Correct owner construction cannot cause this invariant failure. Supply
        // another declaration's inputs to exercise the real request guard.
        let request = ResourceAdmissionRequest::new(
            owner,
            owner.quoted_string_attrs(),
            ResourceAttributeInputs::new(other),
            ValidationEvaluation::None,
            &bindings,
            session,
        );
        let expected = match &request {
            Err(error) => error.to_string(),
            Ok(_) => panic!("mismatched declarations must fail"),
        };
        assert!(expected.contains("canonical resource schema/value pairing"));
        let mut diagnostics = Vec::new();
        let outcome = admission_or_diagnostic(
            &doc.for_diagnostics().unwrap(),
            owner.type_location(),
            request.map(|_| Some(())),
            &mut diagnostics,
        );
        assert!(outcome.is_none());
        assert_eq!(
            diagnostics.len(),
            1,
            "missing evidence must never silently drop admission"
        );
        assert_eq!(diagnostics[0].message, expected);
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 0), Position::new(0, 9))
        );
    });
}
