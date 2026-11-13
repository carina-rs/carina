use super::*;

#[test]
fn r10_provider_finalization_observes_unsaved_imported_module_values() {
    use carina_core::{
        config_loader::SourceOverrides, module_resolver::ModuleResolutionDiagnosticResult,
        paths::ResolvedPath,
    };
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("child");
    std::fs::create_dir(&module).unwrap();
    let root_source = "let child = use { source = './child' }\nlet instance = child {}\nprovider awscc { default_tags = instance.tags }\n";
    let disk = "attributes { tags = { env = 'dev' } }\n";
    let unsaved = "attributes { tags = 'not a map' }\n";
    std::fs::write(directory.path().join("main.crn"), root_source).unwrap();
    std::fs::write(module.join("main.crn"), disk).unwrap();
    let document = create_document_at(unsaved, module.join("main.crn"));
    let engine = test_engine();
    let (old, actual, notices) = engine.with_run_for_test(|run| {
        let base = ResolvedPath::new(directory.path()).unwrap();
        let doc = document.for_diagnostics().unwrap();
        let overrides = SourceOverrides::from([(doc.source_file().to_path_buf(), doc.text())]);
        let parsed =
            carina_core::config_loader::parse_directory_for_metadata_with_source_overrides_report(
                &base,
                run.provider_context.catalog(),
                &overrides,
                &run.schemas,
            )
            .unwrap();
        let mut validation =
            carina_core::test_utils::conversion_validation::<std::convert::Infallible>();
        // Main's previous query forwards the same snapshot to the resolver's
        // module loader. No separate main binary or production mutation.
        let resolution =
            carina_core::module_resolver::resolve_modules_with_diagnostics_and_overrides(
                &parsed,
                &base,
                run.provider_context,
                &run.schemas,
                &[],
                &overrides,
            )
            .into_validation(&mut validation);
        let old = match resolution {
            ModuleResolutionDiagnosticResult::Expanded { mut graph } => {
                carina_core::parser::resolve_expanded_provider_unresolved_attributes_for_analysis(
                    &mut graph,
                    run.provider_context.catalog(),
                )
                .and_then(|()| {
                    carina_core::parser::finalize_provider_configs_for_analysis(&mut graph)
                })
                .err()
                .map(|error| error.to_string())
            }
            ModuleResolutionDiagnosticResult::ConstraintFailed { findings, errors } => panic!(
                "unexpected control constraint failure: {findings:?}, {:?}",
                errors
                    .iter()
                    .map(|error| error.display_relative_to(directory.path()))
                    .collect::<Vec<_>>()
            ),
            ModuleResolutionDiagnosticResult::Unowned { errors } => panic!(
                "unexpected control resolution failure: {:?}",
                errors
                    .iter()
                    .map(|error| error.display_relative_to(directory.path()))
                    .collect::<Vec<_>>()
            ),
        };
        let notices = validation.complete().into_presented();
        assert!(notices.is_empty(), "no-as metadata control: {notices:?}");
        let actual = run
            .finalize_provider_diagnostic(&doc, Some("child/main.crn"), &base)
            .map(|error| error.to_string());
        (old, actual, notices.len())
    });
    let ordinary_analysis = engine.analyze(&document, Some(&module.join("main.crn")));
    println!(
        "R10-FINALIZE {}",
        serde_json::json!({"root":root_source,"module_disk":disk,"module_unsaved":unsaved,"main_query":old,"branch_query":actual,"presented_metadata_findings":notices,"analyze_module_file":ordinary_analysis})
    );
    assert!(
        old.as_ref()
            .is_some_and(|error| error.contains("default_tags")),
        "main control did not reach finalization: {old:?}"
    );
    assert_eq!(
        actual, old,
        "the provider metadata query lost the unsaved module override"
    );
}

#[test]
fn r10_reinstanced_omitted_slots_keep_source_evidence_in_the_lsp_fallback() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};
    use carina_core::validation::admission::attributes::{ResourceAttributeInputs, SlotPrechecks};
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", ResourceSchema::new("Source").validate().unwrap());
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "attr",
                    AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("mock.Name")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = custom_engine(schemas);
    for module in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        let body = "let p = mock.Source {}\nlet sinks = for n in ['a', 'b'] { mock.Sink { attr = p as mock.Name } }\n";
        let source = if module {
            std::fs::create_dir(directory.path().join("child")).unwrap();
            std::fs::write(
                directory.path().join("child/main.crn"),
                format!("arguments {{ unused: String = 'ok' }}\n{body}"),
            )
            .unwrap();
            "let child = use { source = './child' }\nlet first = child {}\nlet second = child {}\n"
        } else {
            body
        };
        let path = directory.path().join("main.crn");
        std::fs::write(&path, source).unwrap();
        let document = create_document_at(source, path.clone());
        let measured = engine.with_run_for_test(|run| {
            let doc = document.for_diagnostics().unwrap();
            let base = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
            let merged = run.parse_and_resolve_merged_with_buffer(&doc, Some("main.crn"), &base).unwrap();
            let source_configuration = carina_core::config_loader::load_buffer_configuration(merged.directory.parsed.parsed(), &run.schemas, run.provider_context.catalog());
            let source_tree = carina_core::parser::AnalysisTree::try_from_file(source_configuration.judged().file()).unwrap();
            let source_map = authored_validation_resources(&source_tree);
            let buffer_tree = carina_core::parser::AnalysisTree::try_from_file(doc.parsed().unwrap()).unwrap();
            let mut selections = Vec::new();
            for resource in buffer_tree.project_resources().filter(|resource| resource.authored_slot("attr").is_ok()) {
                let values = resource.resolved_attributes();
                let (_, inputs) = select_admission_inputs(resource, Some(&source_map), &values, source_configuration.judged().report(), SlotPrechecks::Unreported);
                selections.push(serde_json::json!({"resource":resource.id().to_string(), "value_present":values.contains_key("attr"), "fallback":inputs.authored_evidence().is_err()}));
            }
            let checks::MergedModuleResolution::Expanded { graph } = &merged.module_resolution else { panic!("the production resolver did not expand this fixture"); };
            let expanded_configuration = carina_core::config_loader::load_buffer_configuration(graph.as_ref(), &run.schemas, run.provider_context.catalog());
            let judged = expanded_configuration.judged();
            let expanded_tree = carina_core::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
            let mut rows = Vec::new();
            for resource in expanded_tree.project_resources().filter(|resource|resource.authored_slot("attr").is_ok()) {
                let values = resource.resolved_attributes();
                let for_resource = ResourceAttributeInputs::for_resource(resource, judged.report()).is_ok();
                let slot = resource.authored_slot("attr").unwrap();
                let cursor = slot.cursor();
                let cursor_carries = cursor.as_ref().ok().map(|cursor|cursor.node().judgement().ordinary().is_err());
                // Exercise the report-only constructor on the same real
                // expanded resource; do not invent a missing source report.
                let fallback = ResourceAttributeInputs::from_report(resource, &values, judged.report());
                assert!(fallback.authored_evidence().is_err());
                rows.push(serde_json::json!({"resource":resource.id().to_string(), "runtime_value_present":values.contains_key("attr"), "projection_present":slot.projection().is_some(), "for_resource_succeeded":for_resource, "cursor_carries":cursor_carries, "cursor_error":cursor.err().map(|error|error.to_string())}));
            }
            assert_eq!(rows.len(), if module {4} else {2});
            assert!(rows.iter().all(|row|row["runtime_value_present"]==false && row["for_resource_succeeded"]==true && row["cursor_carries"]==true), "{rows:?}");
            serde_json::json!({"buffer_selections":selections,"expanded_slots":rows})
        });
        let diagnostics = engine.analyze(&document, Some(&path));
        println!("R10-FALLBACK {}", serde_json::json!({"module":module,"source":source,"body":body,"evidence":measured,"lsp":diagnostics}).to_string().replace(directory.path().to_str().unwrap(), "<fixture>"));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
            "{diagnostics:?}"
        );
    }
}
