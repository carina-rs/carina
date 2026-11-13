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
    let missing = inputs
        .authored_evidence()
        .expect_err("report fallback is not authored admission");
    assert_eq!(missing.resource_id(), resource.id());
    assert_eq!(missing.slots().len(), 1);
    assert_eq!(missing.slots()[0].attribute(), "input");
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
    let completeness = judged.report().completeness_report(&coverage);
    assert!(
        completeness
            .unreached_slots()
            .any(|slot| slot.resource_id() == resource.id() && slot.attribute() == "input")
    );
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
fn round5_source_resource_missing_from_report_is_explicitly_unreached() {
    use carina_core::resource::knowledge::KnowledgeContext;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
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
    let previous = carina_core::parser::parse_file(
        "mock.Previous { input = 1 }",
        std::path::Path::new("buffer.crn"),
        &config,
    )
    .unwrap();
    let sealed =
        carina_core::config_loader::load_buffer_configuration(&previous, &schemas, &config);
    // A newer source resource is being checked against a different sealed
    // analysis. Its syntax exists, but that report cannot authorize its slots.
    let current = carina_core::parser::parse_file(
        "mock.Test { input = 'bad' }",
        std::path::Path::new("buffer.crn"),
        &config,
    )
    .unwrap();
    let tree = carina_core::parser::AnalysisTree::try_from_file(&current).unwrap();
    let resource = tree.resources().next().unwrap();
    assert!(
        sealed
            .judged()
            .report()
            .for_resource(resource.id())
            .is_err()
    );
    let map = authored_validation_resources(&tree);
    let values = resource.resolved_attributes();
    let (selected, inputs) =
        select_admission_inputs(resource, Some(&map), &values, sealed.judged().report());
    let missing = inputs
        .authored_evidence()
        .expect_err("source syntax without a matching report is explicitly unavailable");
    assert_eq!(missing.slots().len(), 1);
    let classified = inputs.classify(&KnowledgeContext::validation(), |_, error| {
        panic!("{error}")
    });
    let bindings = carina_core::binding_index::BindingIndex::from_parsed(
        sealed.judged().file(),
        &schemas,
        &config,
    );
    let mut coverage = Default::default();
    carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
        let report = carina_core::validation::admission::attributes::resource_attribute_admissions(
            selected,
            sealed.judged().file(),
            selected.schema(&schemas).unwrap(),
            &classified,
            selected.quoted_string_attrs(),
            &inputs,
            &bindings,
            session,
            &schemas,
            &mut coverage,
        );
        assert_eq!(
            report.errors().count() + report.symbolic_errors().count(),
            1,
            "main's existing type error only: {report:?}"
        );
    });
    let completeness = sealed.judged().report().completeness_report(&coverage);
    assert!(
        completeness
            .unreached_slots()
            .any(|slot| slot.resource_id() == resource.id() && slot.attribute() == "input")
    );
    assert!(
        coverage.is_empty(),
        "report-only checks confer no authored coverage"
    );
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
                parsed.admit(&schemas, session),
                &path,
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

#[test]
fn round6_report_fallback_records_the_matched_authored_slots() {
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let parse = |source| {
        carina_core::parser::parse_file(source, std::path::Path::new("buffer.crn"), &config)
            .unwrap()
    };
    // Independent buffer and expanded parses share a declaration key, never
    // their authored node/use identities. Neither has this report's resource.
    let buffer = parse("mock.Test { input = ('bad') }");
    let expanded = parse("mock.Test { input = ('bad') }");
    let previous = parse("mock.Previous { input = 1 }");
    let schemas = SchemaRegistry::new();
    let sealed =
        carina_core::config_loader::load_buffer_configuration(&previous, &schemas, &config);
    let buffer_tree = carina_core::parser::AnalysisTree::try_from_file(&buffer).unwrap();
    let expanded_tree = carina_core::parser::AnalysisTree::try_from_file(&expanded).unwrap();
    let authored = buffer_tree.resources().next().unwrap();
    let resource = expanded_tree.resources().next().unwrap();
    assert_eq!(
        authored.authored_instance_key(),
        resource.authored_instance_key()
    );
    let map = authored_validation_resources(&buffer_tree);
    let expected =
        carina_core::validation::admission::attributes::ResourceAttributeInputs::from_report(
            authored,
            &authored.resolved_attributes(),
            sealed.judged().report(),
        );
    let (_, actual) = select_admission_inputs(
        resource,
        Some(&map),
        &resource.resolved_attributes(),
        sealed.judged().report(),
    );
    let expected = expected.authored_evidence().unwrap_err();
    let actual = actual.authored_evidence().unwrap_err();
    assert_eq!(
        actual.slots(),
        expected.slots(),
        "unreached identities belong to the authored match"
    );
}

#[test]
fn round7_report_fallback_checks_expanded_resource_with_authored_unreached_slots() {
    use carina_core::resource::knowledge::KnowledgeContext;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use carina_core::validation::admission::attributes::{
        ResourceAttributeInputs, resource_attribute_admissions,
    };
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    // The source address is shared, while resolution has changed the owner.
    // Only the expanded Sink has a per-resource report in this analysis.
    let authored = carina_core::parser::parse_file(
        "arguments { x: Int }\nmock.Source { input = x + 1 }",
        std::path::Path::new("fallback.crn"),
        &config,
    )
    .unwrap();
    let expanded = carina_core::parser::parse_file(
        "arguments { x: Int }\nmock.Sink { input = x + 1 }",
        std::path::Path::new("fallback.crn"),
        &config,
    )
    .unwrap();
    let configuration =
        carina_core::config_loader::load_buffer_configuration(&expanded, &schemas, &config);
    let judged = configuration.judged();
    let authored_tree = carina_core::parser::AnalysisTree::try_from_file(&authored).unwrap();
    let expanded_tree = carina_core::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let source = authored_tree.resources().next().unwrap();
    let rref = expanded_tree.resources().next().unwrap();
    assert_eq!(source.authored_instance_key(), rref.authored_instance_key());
    assert_ne!(source.id(), rref.id());
    let values = rref.resolved_attributes();
    assert!(ResourceAttributeInputs::from_resolved(source, &values, judged.report()).is_err());
    let main_inputs = ResourceAttributeInputs::from_report(rref, &values, judged.report());
    let source_inputs = ResourceAttributeInputs::from_report(source, &values, judged.report());
    let map = authored_validation_resources(&authored_tree);
    let (selected, inputs) = select_admission_inputs(rref, Some(&map), &values, judged.report());
    let bindings =
        carina_core::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &config);
    let admit = |resource, inputs: &ResourceAttributeInputs<_>| {
        let classified = inputs.classify(&KnowledgeContext::validation(), |_, error| {
            panic!("{error}")
        });
        let mut coverage = Default::default();
        let errors =
            carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
                let report = resource_attribute_admissions(
                    resource,
                    judged.file(),
                    rref.schema(&schemas).unwrap(),
                    &classified,
                    rref.quoted_string_attrs(),
                    inputs,
                    &bindings,
                    session,
                    &schemas,
                    &mut coverage,
                );
                report
                    .errors()
                    .map(ToString::to_string)
                    .chain(
                        report
                            .symbolic_errors()
                            .map(|(name, path, error)| format!("{name}:{path}:{error}")),
                    )
                    .collect::<Vec<_>>()
            });
        (errors, coverage)
    };
    let (expected, _) = admit(rref, &main_inputs);
    assert!(
        !expected.is_empty(),
        "main reports the arithmetic type mismatch"
    );
    let (actual, coverage) = admit(selected, &inputs);
    assert_eq!(
        actual, expected,
        "fallback diagnostics must use the expanded resource"
    );
    assert!(std::ptr::eq(selected, rref));
    let source_slots = source_inputs.authored_evidence().unwrap_err().slots();
    assert_eq!(
        inputs.authored_evidence().unwrap_err().slots(),
        source_slots
    );
    let missing = judged
        .report()
        .completeness_report(&coverage)
        .unreached_slots()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(missing, source_slots);
}
