use super::*;

fn check_report_selection(with_empty_map: bool) {
    use carina_core::resource::knowledge::KnowledgeContext;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use carina_core::validation::admission::attributes::{
        ResourceAttributeInputs, resource_attribute_admissions,
    };

    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("input", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    let parsed = carina_core::parser::parse_file(
        "mock.Test { input = 'bad' }",
        std::path::Path::new("test.crn"),
        &config,
    )
    .unwrap();
    let configuration =
        carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &config);
    let judged = configuration.judged();
    let tree = carina_core::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let empty = HashMap::new();
    let values = resource.resolved_attributes();
    let (selected, inputs) = select_admission_inputs(
        resource,
        with_empty_map.then_some(&empty),
        &values,
        judged.report(),
    );
    assert!(std::ptr::eq(selected, resource));
    let bindings =
        carina_core::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &config);
    let admit = |inputs: &ResourceAttributeInputs<_>| {
        let classified = inputs.classify(&KnowledgeContext::validation(), |_, error| {
            panic!("{error}")
        });
        let mut coverage = Default::default();
        let errors =
            carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
                let report = resource_attribute_admissions(
                    selected,
                    judged.file(),
                    selected.schema(&schemas).unwrap(),
                    &classified,
                    selected.quoted_string_attrs(),
                    inputs,
                    &bindings,
                    session,
                    &schemas,
                    &mut coverage,
                );
                report
                    .errors()
                    .chain(report.symbolic_errors().map(|(_, _, error)| error))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            });
        (errors, coverage)
    };
    let (errors, coverage) = admit(&inputs);
    let (expected, _) = admit(&ResourceAttributeInputs::from_report(
        resource,
        &values,
        judged.report(),
    ));
    assert_eq!(errors, expected);
    assert_eq!(errors.len(), 1);
    assert!(coverage.is_empty(), "fallback must use report evidence");
}

#[test]
fn round3_lsp_absent_authored_map_selects_report_inputs() {
    check_report_selection(false);
}

#[test]
fn round3_lsp_empty_authored_map_selects_report_inputs() {
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
            let configuration =
                carina_core::config_loader::load_buffer_configuration(&graph, &schemas, &config);
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
            let tree = carina_core::parser::AnalysisTree::try_from_file(&instances).unwrap();
            let mut inputs = ExpandedDiagnosticValidationInputs {
                configuration: configuration.judged(),
                resources: tree
                    .resources()
                    .map(|resource| (resource.id(), resource))
                    .collect(),
                coverage: Default::default(), // SEAM_ONLY_COVERAGE
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
