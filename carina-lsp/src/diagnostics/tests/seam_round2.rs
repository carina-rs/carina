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
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(
        configuration.file(),
        &schemas,
        &config,
    );
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
        let mut validation =
            carina_core::validation::ConversionValidation::<OrdinaryDiagnostic>::new(
                configuration.report(),
            );
        let outcome = validation.admit_resource(request);
        let errors = outcome
            .values
            .errors()
            .chain(outcome.values.symbolic_errors().map(|(_, _, error)| error))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("expected Int, got String"), "{errors:?}");
    });
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
            let mut instances = configuration.judged().file().clone();
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
                binding_index: carina_core::binding_index::BindingIndex::from_parsed(
                    configuration.judged().file(),
                    &schemas,
                    &config,
                ),
            };
            let resource = tree.resources().next().unwrap();
            let result = inputs.resource_admissions(resource.id(), session);
            actual.push(result.map(|report| {
                report
                    .errors()
                    .chain(report.symbolic_errors().map(|(_, _, error)| error))
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
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(
        configuration.file(),
        &schemas,
        &config,
    );
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
