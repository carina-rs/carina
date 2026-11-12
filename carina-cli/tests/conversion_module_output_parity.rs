use carina_core::{provider::*, schema::*};
use std::{collections::HashMap, sync::Arc};
use tower_lsp::lsp_types::DiagnosticSeverity;

struct ConversionFactory;
impl ProviderFactory for ConversionFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Mock"
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        unimplemented!("validation fixture never constructs a provider")
    }
    fn create_normalizer(
        &self,
        _: &ProviderSession,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let name = || {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("mock.Name")),
                None,
                Some((Some(1), Some(8))),
                None,
            )
        };
        vec![
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("attr", name()).build())
                .attribute(
                    AttributeSchema::optional(
                        "ambiguous",
                        AttributeType::union(vec![name(), name()]),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional(
                        "config",
                        AttributeType::struct_(
                            "Config",
                            vec![
                                StructField::optional("converted", name()).build(),
                                StructField::optional("plain", name()).build(),
                                StructField::optional(
                                    "nested",
                                    AttributeType::struct_(
                                        "Nested",
                                        vec![StructField::optional("bad", name()).build()],
                                    ),
                                )
                                .build(),
                            ],
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Source")
                .attribute(
                    AttributeSchema::optional(
                        "name",
                        AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("mock.OtherName")),
                            None,
                            Some((Some(1), Some(8))),
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "alternate",
                        AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("mock.OtherName")),
                            None,
                            Some((Some(1), Some(8))),
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("number", AttributeType::int()).build())
                .attribute(
                    AttributeSchema::optional(
                        "output",
                        AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("mock.LongName")),
                            None,
                            Some((Some(20), Some(32))),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        ]
    }
}
fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(ConversionFactory)]
}

#[test]
fn conversion_module_output_cli_and_lsp_use_the_same_admission_and_completion() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "arguments { value: String }\nattributes { out: String = value }\n",
    )
    .unwrap();
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine =
        carina_lsp::diagnostics::DiagnosticEngine::new(Arc::new(loaded), vec!["mock".into()]);
    let path = directory.path().join("main.crn");
    for (attributes, rejected) in [
        ("attr = net.out as mock.Name", false),
        ("plain = net.out attr = 'ok' as mock.Name", false),
        ("plain = net.out as mock.Name", true),
    ] {
        let source = format!(
            "let child = use {{ source = './child' }}\nlet net = child {{ value = 'ok' }}\nmock.Sink {{ {attributes} }}\n"
        );
        std::fs::write(&path, &source).unwrap();
        let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
            HashMap::new(),
        );
        let document = carina_lsp::document::Document::new_with_path(
            source.clone(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let lsp = engine.analyze(&document, Some(&path));
        let errors = lsp
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect::<Vec<_>>();
        assert_eq!(
            cli.errors.len(),
            usize::from(rejected),
            "{source}\n{:?}",
            cli.json
        );
        assert_eq!(errors.len(), usize::from(rejected), "{source}\n{lsp:#?}");
        if rejected {
            assert_eq!(cli.json["errors"][0]["code"], "conversion_destination");
            assert_eq!(cli.json["errors"][0]["message"], errors[0].message);
            assert_eq!(
                cli.json["errors"][0]["location"]["range"]["start"]["line"],
                errors[0].range.start.line + 1
            );
            assert_eq!(
                cli.json["errors"][0]["location"]["range"]["start"]["column"],
                errors[0].range.start.character + 1
            );
        }
    }
}

#[test]
fn conversion_unresolved_roots_do_not_poison_for_and_module_siblings() {
    #[derive(Debug)]
    enum Shape {
        For,
        Module,
    }
    for shape in [Shape::For, Shape::Module] {
        for sibling_file in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
            let mut source = match shape {
                Shape::For => {
                    "for item in ['ok', 'fine'] { mock.Sink { attr = item as mock.Name } }\n"
                        .to_owned()
                }
                Shape::Module => {
                    std::fs::create_dir(directory.path().join("child")).unwrap();
                    std::fs::write(
                        directory.path().join("child/main.crn"),
                        "arguments { input: String }\nmock.Sink { attr = input as mock.Name }\n",
                    )
                    .unwrap();
                    "let child = use { source = './child' }\nlet one = child { input = 'ok' }\nlet two = child { input = 'fine' }\n".to_owned()
                }
            };
            let unknown = "let unknown = mock.Unknown { plain = 'unrelated' }\n";
            if sibling_file {
                std::fs::write(directory.path().join("sibling.crn"), unknown).unwrap();
            } else {
                source.push_str(unknown);
            }
            std::fs::write(directory.path().join("main.crn"), &source).unwrap();
            let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
                &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                HashMap::new(),
            );
            let errors = report.json["errors"].as_array().unwrap();
            assert_eq!(errors.len(), 1, "{shape:?}/{sibling_file}: {}", report.json);
            assert!(errors[0]["code"].is_null(), "{}", report.json);
            assert!(
                errors[0]["message"].as_str().unwrap().contains("Unknown"),
                "{}",
                report.json
            );
        }
    }
}

#[test]
fn conversion_module_body_kinds_have_one_presented_owner_and_context() {
    for (body, code) in [
        ("mock.Sink { attr = 'ok' as String }", "conversion_target"),
        (
            "mock.Sink { plain = 'ok' as mock.Name }",
            "conversion_destination",
        ),
        (
            "mock.Sink { attr = 'toolong-for-slot' as mock.Name }",
            "conversion_destination",
        ),
        (
            "mock.Sink { attr = 42 as mock.Name }",
            "conversion_scalar_kind",
        ),
        (
            "mock.Sink { attr = ['ok'] as mock.Name }",
            "conversion_source",
        ),
        (
            "mock.Sink { ambiguous = 'ok' as mock.Name }",
            "conversion_ambiguous_destination",
        ),
        (
            "let producer = mock.Source {}\nmock.Sink { attr = producer.output as mock.Name }",
            "conversion_disjoint",
        ),
        ("let abandoned = 'ok' as mock.Name", "conversion_unused"),
        (
            "mock.Sink { plain = upper('ok' as mock.Name) }",
            "conversion_unsupported_position",
        ),
        (
            "let rejected = if 42 as String { 'ok' } else { 'ok' }",
            "conversion_unsupported_position",
        ),
        (
            r#"mock.Sink { "${'attr' as mock.Name}" = 'ok' }"#,
            "conversion_unsupported_position",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            directory.path().join("main.crn"),
            "let child = use { source = './child' }\nlet net = child { unused = 'ok' }\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("child/main.crn"),
            format!("arguments {{ unused: String }}\n{body}\n"),
        )
        .unwrap();
        let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
            HashMap::new(),
        );
        let errors = report.json["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{body}: {}", report.json);
        assert_eq!(errors[0]["code"], code, "{body}: {}", report.json);
        assert!(
            errors[0]["message"].as_str().unwrap().contains("child"),
            "module context is part of the presented finding: {}",
            report.json
        );
        assert!(
            errors[0]["location"]["file"]
                .as_str()
                .unwrap()
                .ends_with("child/main.crn"),
            "{}",
            report.json
        );
    }
}

#[test]
fn conversion_unused_inventory_waits_for_admission_after_an_ordinary_early_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let mut ordinary = Vec::new();
    for converted in [false, true] {
        let expression = if converted {
            "'ok' as mock.Name"
        } else {
            "'ok'"
        };
        std::fs::write(directory.path().join("main.crn"), format!("provider mock {{}}\nlet abandoned = {expression}\nmock.Sink {{ attr = missing.output }}\n")).unwrap();
        let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &path,
            carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
            HashMap::new(),
        );
        let mut conversions = Vec::new();
        let mut others = Vec::new();
        for error in report.json["errors"].as_array().unwrap() {
            if error["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("conversion_"))
            {
                conversions.push(error);
            } else {
                others.push(error.clone());
            }
        }
        assert!(
            conversions.is_empty(),
            "unused destinations require an admission pass: {}",
            report.json
        );
        ordinary.push(others);
    }
    assert!(!ordinary[0].is_empty());
    assert_eq!(ordinary[0], ordinary[1]);
}

#[test]
fn round3_early_ordinary_failure_does_not_reject_a_valid_conversion_cli_and_lsp() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.crn");
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine =
        carina_lsp::diagnostics::DiagnosticEngine::new(Arc::new(loaded), vec!["mock".into()]);
    for named in [false, true] {
        let mut cli_findings = Vec::new();
        let mut lsp_findings = Vec::new();
        for expression in ["'ok'", "'ok' as mock.Name"] {
            let source = if named {
                format!(
                    "let broken = mock.Sink {{ attr = missing.output }}\nlet accepted = mock.Sink {{ attr = {expression} }}\n"
                )
            } else {
                format!(
                    "mock.Sink {{ attr = missing.output }}\nmock.Sink {{ attr = {expression} }}\n"
                )
            };
            std::fs::write(&path, &source).unwrap();
            let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
                &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                HashMap::new(),
            );
            cli_findings.push(report.json["errors"].clone());
            let document = carina_lsp::document::Document::new_with_path(
                source,
                path.clone(),
                Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            lsp_findings.push(
                diagnostics
                    .into_iter()
                    .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(cli_findings[0], cli_findings[1], "named={named}");
        assert_eq!(lsp_findings[0], lsp_findings[1], "named={named}");
        assert!(!lsp_findings[0].is_empty());
    }
}

#[test]
fn round3_parse_rejection_survives_an_ordinary_early_failure() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("main.crn"), "provider mock {}\nlet invalid = if 42 as String { 'ok' } else { 'ok' }\nmock.Sink { attr = missing.output }\n").unwrap();
    let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        HashMap::new(),
    );
    let errors = report.json["errors"].as_array().unwrap();
    let conversions: Vec<_> = errors
        .iter()
        .filter(|error| {
            error["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("conversion_"))
        })
        .collect();
    assert_eq!(conversions.len(), 1, "{}", report.json);
    assert!(
        conversions[0]["message"]
            .as_str()
            .unwrap()
            .contains("inside this expression"),
        "{}",
        report.json
    );
    assert!(
        errors
            .iter()
            .any(|error| error["message"].as_str().unwrap().contains("missing")),
        "{}",
        report.json
    );
}

fn merge_1b_reference_findings(
    attributes: &str,
) -> (serde_json::Value, Vec<tower_lsp::lsp_types::Diagnostic>) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("sources.crn"),
        "let other = mock.Source { name = 'ok' alternate = 'also' number = 42 }\n",
    )
    .unwrap();
    let source = format!("let sink = mock.Sink {{ {attributes} }}\n");
    let path = directory.path().join("main.crn");
    std::fs::write(&path, &source).unwrap();
    let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        HashMap::new(),
    );
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine =
        carina_lsp::diagnostics::DiagnosticEngine::new(Arc::new(loaded), vec!["mock".into()]);
    let document = carina_lsp::document::Document::new_with_path(
        source,
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let diagnostics = engine
        .analyze(&document, Some(&path))
        .into_iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.severity,
                Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
            ) && !diagnostic.message.starts_with("Unused let binding")
        })
        .collect();
    (report.json, diagnostics)
}

#[test]
fn merge_1b_accepted_reference_claim_suppresses_only_its_reference_rule() {
    for operand in ["other.name", "((other.name))"] {
        let (json, lsp) = merge_1b_reference_findings(&format!("attr = {operand}"));
        let ordinary = json["errors"].as_array().unwrap();
        assert_eq!(ordinary.len(), 1, "{json}");
        assert_eq!(ordinary[0]["code"], serde_json::Value::Null, "{json}");
        assert!(
            ordinary[0]["message"]
                .as_str()
                .unwrap()
                .contains("OtherName"),
            "{json}"
        );
        assert_eq!(lsp.len(), 1, "{lsp:?}");
        assert!(lsp[0].message.contains("OtherName"), "{lsp:?}");
    }
    for operand in [
        "other.name as mock.Name",
        "((other.name as mock.Name))",
        "(other.name) as mock.Name",
    ] {
        let (json, lsp) = merge_1b_reference_findings(&format!("attr = {operand}"));
        assert_eq!(json["status"], "ok", "{json}");
        assert!(
            json.get("errors")
                .is_none_or(|errors| errors.as_array().is_some_and(Vec::is_empty)),
            "{json}"
        );
        assert!(lsp.is_empty(), "{lsp:?}");
    }
}

#[test]
fn merge_1b_rejected_reference_claim_has_one_conversion_diagnostic() {
    for (operand, code) in [
        ("other.number as mock.Name", "conversion_scalar_kind"),
        ("((other.name as mock.OtherName))", "conversion_destination"),
    ] {
        let (json, lsp) = merge_1b_reference_findings(&format!("attr = {operand}"));
        let errors = json["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{json}");
        assert_eq!(errors[0]["code"], code, "{json}");
        assert_eq!(lsp.len(), 1, "{lsp:?}");
        assert_eq!(
            lsp[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(code.into())),
            "{lsp:?}"
        );
    }
}

#[test]
fn merge_1b_claim_preserves_sibling_and_nested_reference_judgements() {
    for (attributes, conversions) in [
        (
            "config = { converted = (other.name as mock.Name) plain = other.name nested = { bad = other.alternate } }",
            0,
        ),
        (
            "config = { converted = (other.number as mock.Name) plain = other.name nested = { bad = other.alternate } }",
            1,
        ),
        (
            "config = ({ plain = other.name nested = { bad = other.alternate } } as mock.Name)",
            1,
        ),
    ] {
        let (json, lsp) = merge_1b_reference_findings(attributes);
        let errors = json["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2 + conversions, "{json}");
        assert_eq!(
            errors
                .iter()
                .filter(|error| error["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("conversion_")))
                .count(),
            conversions,
            "{json}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|error| error["message"].as_str().unwrap().contains("OtherName"))
                .count(),
            2,
            "{json}"
        );
        assert_eq!(lsp.len(), 2 + conversions, "{lsp:?}");
        assert_eq!(
            lsp.iter()
                .filter(|diagnostic| diagnostic.message.contains("OtherName"))
                .count(),
            2,
            "{lsp:?}"
        );
    }
}
