#[path = "support/conversion_totality.rs"]
mod fixtures;
use carina_core::{
    paths::ResolvedPath, provider::loaded_provider_snapshot_from_factories, schema::SchemaRegistry,
};
use fixtures::*;
use std::sync::Arc;

fn validate(directory: &std::path::Path) -> serde_json::Value {
    carina_cli::commands::validate::validate_diagnostics_with_factories(
        &ResolvedPath::new(directory).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        Default::default(),
    )
    .json
}
fn conversion_count(json: &serde_json::Value) -> usize {
    json["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|error| {
            error["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("conversion_"))
        })
        .count()
}
fn internal(text: &str) -> bool {
    text.contains("bug in carina")
        || text.contains("admission did not complete")
        || text.contains("internal_invariant")
}

#[test]
fn round5_generated_p1_covers_every_grammar_context() {
    use std::collections::BTreeSet;
    let grammar = include_str!("../../carina-core/src/parser/carina.pest");
    let rules: BTreeSet<_> = grammar
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) || line.starts_with("//") {
                return None;
            }
            let (name, _) = line.split_once('=')?;
            let name = name.trim();
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                .then_some(name)
        })
        .collect();
    let classified: BTreeSet<_> = CONTEXTS
        .iter()
        .flat_map(|context| context.rules.iter().copied())
        .chain(NON_CONTEXT_RULES.iter().map(|(rule, _)| *rule))
        .collect();
    assert_eq!(
        rules, classified,
        "new grammar rules require an explicit expression-role classification"
    );
    assert!(
        NON_CONTEXT_RULES
            .iter()
            .all(|(_, reason)| !reason.is_empty())
    );
    let catalog =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    let mut cells = 0;
    let mut excluded = 0;
    let mut failures = Vec::new();
    for context in CONTEXTS {
        for evaluation in EVALUATIONS {
            let fixture = match context.fixture(evaluation, true) {
                Ok(fixture) => fixture,
                Err(reason) => {
                    eprintln!("excluded P1 {}/{evaluation:?}: {reason}", context.label);
                    excluded += 1;
                    continue;
                }
            };
            cells += 1;
            let source = &fixture.files["main.crn"];
            let parsed = match carina_core::parser::parse_file(
                source,
                std::path::Path::new("p1.crn"),
                &catalog,
            ) {
                Ok(parsed) => parsed,
                Err(error) => {
                    if let Some(reason) = context.parser_rejection(evaluation) {
                        let mut ordinary = &error;
                        loop {
                            if let carina_core::parser::ParseError::Located { error, .. } = ordinary
                            {
                                ordinary = error;
                                continue;
                            }
                            if let Some(element) = ordinary.element_diagnostic() {
                                ordinary = element.detail();
                                continue;
                            }
                            break;
                        }
                        assert!(
                            matches!(
                                ordinary,
                                carina_core::parser::ParseError::InvalidExpression { .. }
                                    | carina_core::parser::ParseError::InvalidAuthoredExpression { .. }
                            ),
                            "{error:?}"
                        );
                        eprintln!(
                            "ordinary parse rejection {}/{evaluation:?}: {reason}; {error}",
                            context.label
                        );
                    } else {
                        failures.push(format!(
                            "{}/{evaluation:?} did not reach Judge: {error}; {source}",
                            context.label
                        ));
                    }
                    continue;
                }
            };
            let configuration =
                carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
            let report =
                carina_core::validation::ConversionValidation::<std::convert::Infallible>::new(
                    configuration.judged().report(),
                )
                .complete()
                .into_presented();
            let conversions = report
                .iter()
                .filter_map(|finding| match finding {
                    carina_core::validation::PresentedFinding::Conversion(error) => Some(error),
                    carina_core::validation::PresentedFinding::Ordinary(_) => None,
                })
                .collect::<Vec<_>>();
            let expected = match context.placement {
                Placement::Transport => 0,
                Placement::Computed | Placement::Boundary => 1,
            };
            let message_matches = conversions.iter().all(|finding| match context.placement {
                Placement::Transport => false,
                Placement::Boundary => finding
                    .message()
                    .ends_with("not supported at this position yet"),
                Placement::Computed => {
                    finding
                        .message()
                        .ends_with("not supported at this position")
                        || matches!(context.label, "secret" | "nested-convert")
                }
            });
            if conversions.len() != expected
                || !message_matches
                || internal(&carina_core::test_utils::render_presented_for_test(&report))
            {
                failures.push(format!(
                    "{}/{evaluation:?}: expected {expected}; {report:?}; {source}",
                    context.label
                ));
            }
        }
    }
    eprintln!(
        "P1 grammar rules: {}; contexts: {}; cells: {cells}; excluded: {excluded}",
        rules.len(),
        CONTEXTS.len()
    );
    assert_eq!(cells + excluded, CONTEXTS.len() * EVALUATIONS.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn round5_generated_transport_roles_validate_and_lsp() {
    let roles = include_str!("../../carina-core/src/parser/retained/conversion/flow.rs")
        .split_once("pub(crate) enum TransportRole {")
        .unwrap()
        .1
        .split_once('}')
        .unwrap()
        .0
        .lines()
        .filter_map(|line| line.trim().strip_suffix(','))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        roles,
        TRANSPORT_EXAMPLES
            .iter()
            .map(|(role, position)| {
                assert!(
                    POSITIONS
                        .iter()
                        .any(|case| case.label() == position.label())
                );
                *role
            })
            .collect(),
        "every transport role needs a matrix fixture"
    );
    let mut cells = 0;
    for position in POSITIONS {
        for known in [true, false] {
            for invalid in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let generated = fixture(position, known, invalid, true);
                generated.write(directory.path());
                let cli = validate(directory.path());
                let expected = usize::from(known && invalid);
                assert_eq!(
                    conversion_count(&cli),
                    expected,
                    "{position:?}/{known}/{invalid}: {cli}"
                );
                assert!(!internal(&cli.to_string()), "{cli}");
                if known && !invalid {
                    assert_eq!(cli["status"], "ok", "{cli}");
                }
                let diagnostics = lsp_findings(&generated, directory.path());
                let conversions = diagnostics.iter().filter(|finding| matches!(&finding.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code.starts_with("conversion_"))).count();
                assert_eq!(
                    conversions, expected,
                    "{position:?}/{known}/{invalid}: {diagnostics:?}"
                );
                assert!(
                    diagnostics
                        .iter()
                        .all(|finding| !internal(&finding.message)),
                    "{diagnostics:?}"
                );
                if !known {
                    fixture(position, known, invalid, false).write(directory.path());
                    let twin = validate(directory.path());
                    let ordinary = |json: &serde_json::Value| {
                        json["errors"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|error| (error["message"].clone(), error["code"].clone()))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        ordinary(&cli),
                        ordinary(&twin),
                        "schema-None retains main's ordinary finding"
                    );
                    assert!(
                        diagnostics.iter().any(|finding| finding.severity
                            == Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)),
                        "{diagnostics:?}"
                    );
                }
                cells += 2;
            }
        }
    }
    assert_eq!(cells, POSITIONS.len() * 2 * 2 * 2);
}

#[tokio::test]
async fn round5_accepted_conversion_uses_production_plan_and_apply() {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    for position in POSITIONS {
        if matches!(position, Position::DataSource) {
            eprintln!(
                "excluded apply data-source: native fixture has no Query read implementation"
            );
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        fixture(position, true, false, true).write(directory.path());
        carina_cli::commands::init::run_init(
            &runtime,
            directory.path(),
            false,
            false,
            false,
            false,
        )
        .await
        .unwrap();
        carina_cli::test_support::run_plan_with_factories(
            &runtime,
            directory.path(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        )
        .await
        .unwrap();
        carina_cli::commands::apply::run_apply_with_factories_for_test(
            &runtime,
            directory.path(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn round5_read_only_runs_judge_placement_without_slot_admission() {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let directory = tempfile::tempdir().unwrap();
    fixture(Position::Root, true, false, false).write(directory.path());
    carina_cli::commands::init::run_init(&runtime, directory.path(), false, false, false, false)
        .await
        .unwrap();
    for args in [
        ["destroy", "--auto-approve", "."],
        ["state", "refresh", "."],
    ] {
        let mut twins = Vec::new();
        for converted in [false, true] {
            fixture(Position::Root, true, false, converted).write(directory.path());
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
                .current_dir(directory.path())
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .args(args)
                .output()
                .unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!internal(&text), "{text}");
            twins.push((output.status.code(), text));
        }
        assert_eq!(twins[0], twins[1], "{args:?}");
        let bad = Fixture::source("mock.Sink { attr = upper('ok' as mock.Name) }\n".into());
        bad.write(directory.path());
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            text.matches("conversion is not supported at this position")
                .count(),
            1,
            "{args:?}: {text}"
        );
    }
}

fn lsp_findings(
    generated: &Fixture,
    directory: &std::path::Path,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine =
        carina_lsp::diagnostics::DiagnosticEngine::new(Arc::new(loaded), vec!["mock".into()]);
    generated
        .files
        .iter()
        .flat_map(|(file, source)| {
            let path = directory.join(file);
            let document = carina_lsp::document::Document::new_with_path(
                source.clone(),
                path.clone(),
                Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            engine.analyze(&document, Some(&path))
        })
        .collect()
}

#[test]
fn merge_require_keeps_an_unrelated_conversion_inventory_owner() {
    let directory = tempfile::tempdir().unwrap();
    let mut outputs = Vec::new();
    for converted in [true, false] {
        let mut generated = fixture(Position::Root, true, false, converted);
        let source = generated.files.get_mut("main.crn").unwrap();
        *source = source.replace("let sink = ", "");
        source.push_str("require 2 == 2, 'root guard'\n");
        generated.write(directory.path());
        let cli = validate(directory.path());
        assert_eq!(cli["status"], "ok", "{cli}");
        assert_eq!(cli["resource_count"], 1, "{cli}");
        assert!(cli.get("errors").is_none(), "{cli}");
        assert!(cli.get("warnings").is_none(), "{cli}");
        outputs.push(cli);
        let diagnostics = lsp_findings(&generated, directory.path());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
    assert_eq!(outputs[0], outputs[1], "no-as twin preserves main's output");
}

fn require_validation_fixture(module: bool, condition: &str, resource: &str) -> Fixture {
    let mut generated = fixture(Position::Root, true, false, false);
    let requirement = format!("require {condition}, 'require merge guard'\n");
    if module {
        generated.files.insert(
            "child/main.crn".into(),
            format!("arguments {{ unused: String }}\n{requirement}{resource}\n"),
        );
        generated.files.insert(
            "main.crn".into(),
            "let child = use { source = './child' }\nlet net = child { unused = 'ok' }\n".into(),
        );
    } else {
        generated.files.insert("guards.crn".into(), requirement);
    }
    generated
}

#[test]
fn merge_require_conversions_remain_ordinary_syntax_errors() {
    use carina_core::parser::{ParseError, ProviderContext, parse_file};
    use tower_lsp::lsp_types::DiagnosticSeverity;
    for module in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut cli_shapes = Vec::new();
        let mut lsp_shapes = Vec::new();
        for suffix in ["as mock.Name", "unexpected"] {
            let generated = require_validation_fixture(
                module,
                &format!("('ok' {suffix}) == 'ok'"),
                "mock.Sink { attr = 'ok' }",
            );
            generated.write(directory.path());
            let cli = validate(directory.path());
            assert_eq!(cli["status"], "error", "{module}/{suffix}: {cli}");
            assert_eq!(conversion_count(&cli), 0, "{cli}");
            assert!(!internal(&cli.to_string()), "{cli}");
            let errors = cli["errors"].as_array().unwrap();
            assert_eq!(errors.len(), 1, "{cli}");
            assert!(
                errors[0]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("Syntax error:"),
                "{cli}"
            );
            assert!(errors[0]["code"].is_null(), "{cli}");
            assert_eq!(errors[0]["severity"], "error", "{cli}");
            assert!(errors[0]["location"]["range"]["start"].is_object(), "{cli}");
            cli_shapes.push((
                errors[0]["code"].clone(),
                errors[0]["severity"].clone(),
                errors[0]["location"].clone(),
            ));

            let file = if module {
                "child/main.crn"
            } else {
                "guards.crn"
            };
            let parsed = parse_file(
                &generated.files[file],
                &directory.path().join(file),
                &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            );
            assert!(
                matches!(&parsed, Err(ParseError::Located { error, .. }) if matches!(error.as_ref(), ParseError::Syntax(_))),
                "unexpected token must retain main's ParseError::Syntax class: {parsed:?}"
            );
            let diagnostics = lsp_findings(&generated, directory.path());
            assert!(!diagnostics.is_empty(), "{module}/{suffix}");
            assert!(
                !diagnostics
                    .iter()
                    .any(|diagnostic| internal(&diagnostic.message)),
                "{diagnostics:?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.code.is_none()),
                "{diagnostics:?}"
            );
            // Main's syntax recovery may also diagnose an unloaded dotted
            // type in the invalid buffer. Compare the parser diagnostics,
            // while every diagnostic must remain ordinary and non-internal.
            let errors: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity == Some(DiagnosticSeverity::ERROR)
                        && diagnostic.message.contains("expected")
                })
                .collect();
            assert!(!errors.is_empty(), "{diagnostics:?}");
            lsp_shapes.push(
                errors
                    .iter()
                    .map(|diagnostic| {
                        (
                            diagnostic.severity,
                            diagnostic.code.clone(),
                            diagnostic.source.clone(),
                            diagnostic.range.start,
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(
            cli_shapes[0], cli_shapes[1],
            "as is the same unexpected-token diagnostic shape: {module}"
        );
        assert_eq!(
            lsp_shapes[0], lsp_shapes[1],
            "as is the same unexpected-token diagnostic shape: {module}"
        );
    }
}

#[test]
fn r11_module_require_failure_keeps_already_judged_static_conversion_findings() {
    let directory = tempfile::tempdir().unwrap();
    for passes in [false, true] {
        let condition = if passes { "true" } else { "false" };
        require_validation_fixture(true, condition, "mock.Sink { attr = 42 as mock.Name }")
            .write(directory.path());
        let cli = validate(directory.path());
        assert_eq!(cli["status"], "error", "{cli}");
        assert!(!internal(&cli.to_string()), "{cli}");
        let errors = cli["errors"].as_array().unwrap();
        assert_eq!(errors.len(), if passes { 1 } else { 2 }, "{cli}");
        assert_eq!(conversion_count(&cli), 1, "{cli}");
        let conversion = errors
            .iter()
            .find(|finding| finding["code"] == "conversion_scalar_kind")
            .unwrap();
        assert!(
            conversion["location"]["file"]
                .as_str()
                .unwrap()
                .ends_with("child/main.crn")
        );
        if !passes {
            let ordinary = errors
                .iter()
                .filter(|finding| finding["code"].is_null())
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                ordinary[0]["message"],
                "module 'child' (call 'net'): arguments : require merge guard"
            );
            require_validation_fixture(true, condition, "mock.Sink { attr = 'ok' }")
                .write(directory.path());
            let control = validate(directory.path());
            assert_eq!(
                ordinary,
                *control["errors"].as_array().unwrap(),
                "ordinary require diagnostics retain main's output"
            );
        }
    }
}

#[test]
fn r11_generated_source_type_owner_stage_matches_the_no_as_twin() {
    let check = |label: &str, twin: Fixture, converted: Fixture, conversion_expected: usize| {
        let directory = tempfile::tempdir().unwrap();
        twin.write(directory.path());
        let ordinary = validate(directory.path());
        let ordinary_lsp = lsp_findings(&twin, directory.path());
        converted.write(directory.path());
        let checked = validate(directory.path());
        let checked_lsp = lsp_findings(&converted, directory.path());
        assert_eq!(
            conversion_count(&checked),
            conversion_expected,
            "{label}: {checked}"
        );
        if ordinary["status"] == "error" {
            assert_eq!(
                checked["status"], "error",
                "{label}: twin={ordinary}; as={checked}"
            );
        }
        let error = Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR);
        if ordinary_lsp.iter().any(|finding| finding.severity == error) {
            assert!(
                checked_lsp.iter().any(|finding| finding.severity == error),
                "{label}: twin={ordinary_lsp:?}; as={checked_lsp:?}"
            );
        }
        let conversions = checked_lsp.iter().filter(|finding| matches!(&finding.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code.starts_with("conversion_"))).count();
        assert_eq!(conversions, conversion_expected, "{label}: {checked_lsp:?}");
        assert!(
            !internal(&checked.to_string())
                && checked_lsp
                    .iter()
                    .all(|finding| !internal(&finding.message))
        );
        eprintln!(
            "owner-stage {label}: twin CLI={} LSP-errors={}; as CLI={} LSP-errors={}",
            ordinary["status"],
            ordinary_lsp
                .iter()
                .filter(|finding| finding.severity == error)
                .count(),
            checked["status"],
            checked_lsp
                .iter()
                .filter(|finding| finding.severity == error)
                .count()
        );
    };
    let mut cells = 0;
    for position in POSITIONS {
        for known in [true, false] {
            let twin = fixture(position, known, true, false);
            let mut converted = fixture(position, known, true, true);
            for source in converted.files.values_mut() {
                *source = source.replace("as mock.Name", "as mock.Other");
            }
            let expected = if known {
                converted
                    .files
                    .values()
                    .map(|source| source.matches(" as mock.Other").count())
                    .sum()
            } else {
                0
            };
            check(
                &format!("{position:?}/schema={known}"),
                twin,
                converted,
                expected,
            );
            cells += 4;
        }
    }
    for position in OWNER_POSITIONS {
        check(
            position.label(),
            position.fixture(false),
            position.fixture(true),
            1,
        );
        cells += 4;
    }
    assert_eq!(cells, (POSITIONS.len() * 2 + OWNER_POSITIONS.len()) * 4);
    eprintln!("source-type owner-stage matrix: {cells} surface/form cells");
}

#[test]
fn r11_module_template_knowledge_cannot_reject_a_known_instance_union_value() {
    for called in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut generated = Fixture::source(format!(
            "let child = use {{ source = './child' }}\n{}",
            if called {
                "let instance = child { value = 'ok' }\n"
            } else {
                ""
            }
        ));
        generated.files.insert(
            "child/main.crn".into(),
            "arguments { value: String }\nmock.Sink { choice = value as mock.Name }\n".into(),
        );
        generated.write(directory.path());
        let cli = validate(directory.path());
        // Compare the same root/instance scope as CLI. A child document opened
        // standalone has no caller's argument values and remains a pending union.
        let root_document = Fixture {
            files: std::collections::BTreeMap::from([(
                "main.crn".into(),
                generated.files["main.crn"].clone(),
            )]),
        };
        let lsp = lsp_findings(&root_document, directory.path());
        assert_eq!(cli["status"], "ok", "called={called}: {cli}");
        assert!(lsp.iter().all(|finding| !matches!(&finding.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code.starts_with("conversion_"))), "called={called}: {lsp:?}");
    }
}
