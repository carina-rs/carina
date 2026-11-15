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
                parsed.admit(&run.schemas, run.provider_context),
                &base,
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
    let ordinary_analysis =
        crate::test_support::Diagnostics::from_schemas(directory.path(), SchemaRegistry::new())
            .analyze(&document, Some(&module.join("main.crn")));
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
fn r16_reinstanced_omitted_sources_keep_the_twins_ordinary_gate() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};
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
    let engine = custom_engine(schemas.clone());
    for module in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        if module {
            std::fs::create_dir(directory.path().join("child")).unwrap();
        }
        let mut observations = Vec::new();
        for converted in [false, true] {
            let operand = if converted { "p as mock.Name" } else { "p" };
            let body = format!(
                "let p = mock.Source {{}}\nlet sinks = for n in ['a', 'b'] {{ mock.Sink {{ attr = {operand} }} }}\n"
            );
            let source = if module {
                std::fs::write(
                    directory.path().join("child/main.crn"),
                    format!("arguments {{ unused: String = 'ok' }}\n{body}"),
                )
                .unwrap();
                "let child = use { source = './child' }\nlet first = child {}\nlet second = child {}\n".to_owned()
            } else {
                body.clone()
            };
            let path = directory.path().join("main.crn");
            std::fs::write(&path, &source).unwrap();
            let document = create_document_at(&source, path.clone());
            let resolution = engine.with_run_for_test(|run| {
                let doc = document.for_diagnostics().unwrap();
                let base = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
                let merged = run
                    .parse_and_resolve_merged_with_buffer(&doc, Some("main.crn"), &base)
                    .unwrap();
                match &merged.module_resolution {
                    checks::MergedModuleResolution::Unowned { errors } => errors
                        .iter()
                        .map(|error| error.display_relative_to(directory.path()))
                        .collect::<Vec<_>>(),
                    checks::MergedModuleResolution::ConstraintFailed { findings, errors } => {
                        panic!(
                            "unexpected constraint failure: {findings:?}; {:?}",
                            errors
                                .iter()
                                .map(|error| error.display_relative_to(directory.path()))
                                .collect::<Vec<_>>()
                        )
                    }
                    checks::MergedModuleResolution::Unanalyzed => {
                        panic!("unexpected analysis failure: {:?}", merged.analysis_errors)
                    }
                    checks::MergedModuleResolution::Expanded { .. } => {
                        panic!("the ordinary resource-handle precheck must prevent this expansion")
                    }
                }
            });
            // The new twin precheck reports the resource handle before expansion.
            // This former fallback reproducer can no longer reach an admitting walk.
            assert!(
                resolution
                    .iter()
                    .any(|message| message.contains("expected mock.Name, got mock.Source")),
                "{resolution:?}"
            );
            let diagnostics =
                crate::test_support::Diagnostics::from_schemas(directory.path(), schemas.clone())
                    .analyze(&document, Some(&path));
            println!("R16-OMITTED {}", serde_json::json!({"module":module,"converted":converted,"source":source,"body":body,"resolution":resolution,"lsp":diagnostics}).to_string().replace(directory.path().to_str().unwrap(), "<fixture>"));
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.message.contains("conversion")
                        && !diagnostic.message.contains("bug in carina")),
                "{diagnostics:?}"
            );
            let errors = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
                .map(|diagnostic| (diagnostic.message.clone(), diagnostic.code.clone()))
                .collect::<Vec<_>>();
            assert!(!errors.is_empty(), "{diagnostics:?}");
            observations.push((resolution, errors));
        }
        assert_eq!(
            observations[1], observations[0],
            "module={module}: the as form must preserve the ordinary twin's gate"
        );
    }
}

#[test]
fn r18_provider_finalization_preserves_mains_forwarded_template_query() {
    use carina_core::{
        config_loader::SourceOverrides, module_resolver::ModuleResolutionDiagnosticResult,
        paths::ResolvedPath,
    };
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("child");
    std::fs::create_dir(&module).unwrap();
    let root = "arguments { flag: Bool }\nlet child = use { source = './child' }\nlet instance = child { flag = flag }\nprovider awscc { default_tags = instance.tags }\n";
    let child = "arguments { flag: Bool }\nlet optional = if flag { mock.test.Thing { value = 'present' } }\nattributes { tags = 'not a map' }\n";
    std::fs::write(directory.path().join("main.crn"), root).unwrap();
    std::fs::write(module.join("main.crn"), child).unwrap();
    let document = create_document_at(root, directory.path().join("main.crn"));
    let engine = test_engine();
    let (old, actual, strict_error) = engine.with_run_for_test(|run| {
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
        let strict_error = carina_core::module_resolver::resolve_modules_with_config_and_overrides(
            parsed.clone().admit(&run.schemas, run.provider_context),
            &base,
            &overrides,
        )
        .err()
        .map(|error| error.to_string());
        let mut validation =
            carina_core::test_utils::conversion_validation::<std::convert::Infallible>();
        let resolution =
            carina_core::module_resolver::resolve_modules_with_diagnostics_and_overrides(
                parsed.admit(&run.schemas, run.provider_context),
                &base,
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
                "unexpected constraints: {findings:?}, {} residual errors",
                errors.len()
            ),
            ModuleResolutionDiagnosticResult::Unowned { errors } => panic!(
                "unexpected resolution errors: {:?}",
                errors
                    .iter()
                    .map(|error| error.display_relative_to(directory.path()))
                    .collect::<Vec<_>>()
            ),
        };
        assert!(validation.complete().into_presented().is_empty());
        let actual = run
            .finalize_provider_diagnostic(&doc, Some("main.crn"), &base)
            .map(|error| error.to_string());
        (old, actual, strict_error)
    });
    println!(
        "R18-METADATA {}",
        serde_json::json!({"root":root,"child":child,"main_query":old,"branch_query":actual,"strict_error":strict_error})
    );
    assert!(
        strict_error
            .as_deref()
            .is_some_and(|error| error.contains("deferred module conditionals")),
        "strict control: {strict_error:?}"
    );
    assert!(
        old.as_deref()
            .is_some_and(|error| error.contains("default_tags must resolve to a map")),
        "main diagnostic control: {old:?}"
    );
    assert_eq!(
        actual, old,
        "metadata finalization must preserve main's diagnostic expansion"
    );
}
