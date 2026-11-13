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
    json.get("errors")
        .and_then(serde_json::Value::as_array)
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
        || text.contains("conversion admission did not complete")
        || text.contains("internal_invariant")
}

#[test]
fn round4_generated_totality_validate_and_lsp() {
    let twin_directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/conversion-totality-no-as");
    let emitted = emit_twins(&twin_directory);
    eprintln!("no-as twins ({emitted}): {}", twin_directory.display());
    let mut cells = 0;
    let mut failures = Vec::new();
    for position in POSITIONS {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                // Read-only is an entry kind, not a mode a validating/LSP run can select.
                if matches!(liveness, Liveness::ReadOnly) {
                    continue;
                }
                if let Some(reason) = resource_cell_exclusion(position, liveness) {
                    eprintln!(
                        "excluded validate/LSP {position:?}/{liveness:?}/{quality:?}: {reason}"
                    );
                    continue;
                }
                let directory = tempfile::tempdir().unwrap();
                let generated = fixture(position, liveness, quality, true);
                generated.write(directory.path());
                let expected = usize::from(
                    matches!(quality, Quality::Invalid) && liveness.has_live_destination(),
                );
                let key = format!(
                    "{}/{}/{}",
                    position.label(),
                    liveness.label(),
                    quality.label()
                );
                let cli = validate(directory.path());
                if matches!(quality, Quality::Valid)
                    && !matches!(
                        liveness,
                        Liveness::DestinationUnknown | Liveness::OrdinaryStopped
                    )
                    && cli["status"] != "ok"
                {
                    failures.push(format!("validate valid fixture {key}: {cli}"));
                }
                if conversion_count(&cli) != expected || internal(&cli.to_string()) {
                    failures.push(format!("validate {key}: expected {expected}; {cli}"));
                }
                cells += 1;

                let loaded = loaded_provider_snapshot_from_factories(
                    Arc::new(factories()),
                    SchemaRegistry::new(),
                    carina_core::EnvironmentSnapshot::empty(),
                );
                let engine = carina_lsp::diagnostics::DiagnosticEngine::new(
                    Arc::new(loaded),
                    vec!["mock".into()],
                );
                let mut conversion_diagnostics = Vec::new();
                let mut internal_diagnostics = Vec::new();
                for (file, source) in &generated.files {
                    let path = directory.path().join(file);
                    let document = carina_lsp::document::Document::new_with_path(
                        source.clone(),
                        path.clone(),
                        Arc::new(carina_core::parser::ProviderContext::new(
                            carina_core::EnvironmentSnapshot::empty(),
                        )),
                    );
                    for diagnostic in engine.analyze(&document, Some(&path)) {
                        if let Some(tower_lsp::lsp_types::NumberOrString::String(code)) =
                            &diagnostic.code
                        {
                            // Opening a module's file alone gives it its own directory
                            // scope. Only live-instance cells ask that standalone scope
                            // to display the module-owned rejection; an unopened inactive
                            // template belongs to the importing root's resolver scope.
                            let owns_scope = !file.starts_with("child/")
                                || matches!(liveness, Liveness::ModuleInstance);
                            if code.starts_with("conversion_") && owns_scope {
                                conversion_diagnostics.push(diagnostic.clone());
                            }
                        }
                        if internal(&diagnostic.message) {
                            internal_diagnostics.push((file.clone(), diagnostic));
                        }
                    }
                }
                if conversion_diagnostics.len() != expected || !internal_diagnostics.is_empty() {
                    failures.push(format!("lsp {key}: expected {expected}; conversion={conversion_diagnostics:?}; internal={internal_diagnostics:?}; CLI={cli}"));
                }
                if matches!(quality, Quality::Valid) {
                    fixture(position, liveness, quality, false).write(directory.path());
                    let twin = validate(directory.path());
                    if cli != twin {
                        failures.push(format!("no-as JSON twin {key}: as={cli}; twin={twin}"));
                    }
                }
                cells += 1;
            }
        }
    }
    eprintln!(
        "totality cells: {cells}; excluded: 44 validate/LSP read-only cells (read-only is a different entry kind)"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(cells, (11 * 12 - 1) * 2 * 2);
}

async fn production_mock_matrix(position: Position) {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let mut cells = 0;
    let mut excluded = 0;
    let mut accepted_live_plans = 0;
    let mut accepted_live_applies = 0;
    let mut failures = Vec::new();
    for liveness in LIVENESS {
        for quality in QUALITIES {
            let generated = match apply_fixture(position, liveness, quality, true) {
                Ok(fixture) => fixture,
                Err(reason) => {
                    eprintln!(
                        "excluded apply {}/{}/{}: {reason}",
                        position.label(),
                        liveness.label(),
                        quality.label()
                    );
                    excluded += 1;
                    continue;
                }
            };
            let directory = tempfile::tempdir().unwrap();
            generated.write(directory.path());
            let key = format!(
                "{}/{}/{}",
                position.label(),
                liveness.label(),
                quality.label()
            );
            eprintln!("production apply cell: {key}");
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
            let expected =
                usize::from(matches!(quality, Quality::Invalid) && liveness.has_live_destination());
            let live_valid = matches!(quality, Quality::Valid) && liveness.has_live_destination();
            let plan = if live_valid {
                carina_cli::test_support::run_plan_with_factories(
                    &runtime,
                    directory.path(),
                    carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                )
                .await
            } else {
                Ok(false)
            };
            let plan_text = plan
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            let apply = carina_cli::commands::apply::run_apply_with_factories_for_test(
                &runtime,
                directory.path(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
            )
            .await;
            let text = apply
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            let count = text.matches("conversion operand").count()
                + text.matches("checked conversion failed").count();
            if count != expected || internal(&text) || internal(&plan_text) {
                failures.push(format!(
                    "production apply {key}: expected {expected}; plan={plan_text}; apply={text}"
                ));
            }
            if matches!(quality, Quality::Valid) && liveness.has_live_destination() {
                accepted_live_plans += usize::from(plan.is_ok());
                accepted_live_applies += usize::from(apply.is_ok());
                if plan.is_err() || apply.is_err() {
                    failures.push(format!("accepted live conversion must plan and apply {key}: plan={plan_text}; apply={text}"));
                }
            }
            cells += 1;
        }
    }
    eprintln!(
        "production apply cells: {cells}; excluded: {excluded}; accepted live plans/applies: {accepted_live_plans}/{accepted_live_applies}"
    );
    assert_eq!(cells + excluded, 13 * 2);
    let supported = match position {
        Position::Root
        | Position::Parens
        | Position::ListItem
        | Position::MapValue
        | Position::StructField
        | Position::IfBranch
        | Position::Coalesce
        | Position::Alias
        | Position::Union
        | Position::Secret => true,
        Position::DataSource => false,
    };
    assert_eq!(cells, if supported { 24 } else { 0 });
    assert_eq!(accepted_live_plans, if supported { 2 } else { 0 });
    assert_eq!(accepted_live_applies, if supported { 2 } else { 0 });
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

macro_rules! apply_rows {
    ($($name:ident: $position:ident),+ $(,)?) => { $(
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() { production_mock_matrix(Position::$position).await; }
    )+ };
}
apply_rows! {
    round4_generated_apply_root: Root, round4_generated_apply_parens: Parens,
    round4_generated_apply_list_item: ListItem, round4_generated_apply_map_value: MapValue,
    round4_generated_apply_struct_field: StructField, round4_generated_apply_if_branch: IfBranch,
    round4_generated_apply_coalesce: Coalesce, round4_generated_apply_alias: Alias,
    round4_generated_apply_union: Union, round4_generated_apply_secret: Secret,
    round4_generated_apply_data_source: DataSource,
}

#[tokio::test]
async fn round4_generated_totality_plan() {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let mut cells = 0;
    let mut failures = Vec::new();
    for position in POSITIONS {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                if matches!(liveness, Liveness::ReadOnly) {
                    continue;
                }
                if let Some(reason) = resource_cell_exclusion(position, liveness) {
                    eprintln!("excluded plan {position:?}/{liveness:?}/{quality:?}: {reason}");
                    continue;
                }
                let directory = tempfile::tempdir().unwrap();
                fixture(position, liveness, quality, true).write(directory.path());
                let result = carina_cli::test_support::run_plan_with_factories(
                    &runtime,
                    directory.path(),
                    carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                )
                .await;
                let text = match result {
                    Ok(_) => String::new(),
                    Err(error) => error.to_string(),
                };
                let count = text.matches("conversion operand").count()
                    + text.matches("checked conversion failed").count();
                let expected = usize::from(
                    matches!(quality, Quality::Invalid) && liveness.has_live_destination(),
                );
                if count != expected || internal(&text) {
                    failures.push(format!(
                        "plan {}/{}/{}: expected {expected}; {text}",
                        position.label(),
                        liveness.label(),
                        quality.label()
                    ));
                }
                cells += 1;
            }
        }
    }
    eprintln!("plan totality cells: {cells}; excluded: 22 read-only cells (different entry)");
    assert_eq!(cells, (11 * 12 - 1) * 2);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn command(directory: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .unwrap()
}
fn output(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test]
async fn round4_generated_totality_destroy_and_state_are_read_only() {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let mut cells = 0;
    let mut failures = Vec::new();
    for position in POSITIONS {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                if let Some(reason) = resource_cell_exclusion(position, liveness) {
                    eprintln!(
                        "excluded destroy/state {position:?}/{liveness:?}/{quality:?}: {reason}"
                    );
                    continue;
                }
                let directory = tempfile::tempdir().unwrap();
                fixture(position, liveness, quality, false).write(directory.path());
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
                for arguments in [
                    vec!["destroy", "--auto-approve", "."],
                    vec!["state", "refresh", "."],
                ] {
                    let mut outcomes = Vec::new();
                    for converted in [false, true] {
                        fixture(position, liveness, quality, converted).write(directory.path());
                        let result = command(directory.path(), &arguments);
                        let text = output(&result);
                        if internal(&text)
                            || text.contains("conversion operand")
                            || text.contains("checked conversion failed")
                            || text.contains("conversion is not supported")
                        {
                            failures.push(format!(
                                "{arguments:?} {}/{}/{}: {text}",
                                position.label(),
                                liveness.label(),
                                quality.label()
                            ));
                        }
                        outcomes.push((result.status.code(), text));
                    }
                    if matches!(quality, Quality::Valid) && outcomes[0] != outcomes[1] {
                        failures.push(format!(
                            "read-only twin mismatch {arguments:?} {}/{}/{}: {outcomes:?}",
                            position.label(),
                            liveness.label(),
                            quality.label()
                        ));
                    }
                    cells += 1;
                }
            }
        }
    }
    eprintln!("destroy/state totality cells: {cells}");
    assert_eq!(cells, (11 * 13 - 1) * 2 * 2);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn round4_generated_boundary_totality_is_independent_of_activation() {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let mut cells = 0;
    let mut excluded = 0;
    let mut failures = Vec::new();
    for boundary in BOUNDARIES {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                let generated = match boundary_fixture(boundary, liveness, quality, true) {
                    Ok(fixture) => fixture,
                    Err(reason) => {
                        eprintln!(
                            "excluded boundary {}/{}/{}: {reason}",
                            boundary.label(),
                            liveness.label(),
                            quality.label()
                        );
                        excluded += 6;
                        continue;
                    }
                };
                let directory = tempfile::tempdir().unwrap();
                fixture(Position::Root, Liveness::Live, Quality::Valid, false)
                    .write(directory.path());
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
                generated.write(directory.path());
                let key = format!(
                    "{}/{}/{}",
                    boundary.label(),
                    liveness.label(),
                    quality.label()
                );
                if matches!(liveness, Liveness::ReadOnly) {
                    excluded += 3;
                } else {
                    let json = validate(directory.path());
                    if conversion_count(&json) != 1
                        || internal(&json.to_string())
                        || !json
                            .to_string()
                            .contains("conversion is not supported at this position yet")
                    {
                        failures.push(format!("boundary validate {key}: {json}"));
                    }
                    cells += 1;
                    let result = carina_cli::test_support::run_plan_with_factories(
                        &runtime,
                        directory.path(),
                        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                    )
                    .await;
                    let text = match result {
                        Ok(_) => String::new(),
                        Err(error) => error.to_string(),
                    };
                    if text
                        .matches("conversion is not supported at this position yet")
                        .count()
                        != 1
                        || internal(&text)
                    {
                        failures.push(format!("boundary plan {key}: {text}"));
                    }
                    cells += 1;
                    let loaded = loaded_provider_snapshot_from_factories(
                        Arc::new(factories()),
                        SchemaRegistry::new(),
                        carina_core::EnvironmentSnapshot::empty(),
                    );
                    let engine = carina_lsp::diagnostics::DiagnosticEngine::new(
                        Arc::new(loaded),
                        vec!["mock".into()],
                    );
                    let mut count = 0;
                    for (file, source) in &generated.files {
                        let path = directory.path().join(file);
                        let document = carina_lsp::document::Document::new_with_path(
                            source.clone(),
                            path.clone(),
                            Arc::new(carina_core::parser::ProviderContext::new(
                                carina_core::EnvironmentSnapshot::empty(),
                            )),
                        );
                        for diagnostic in engine.analyze(&document, Some(&path)) {
                            count += usize::from(
                                diagnostic
                                    .message
                                    .contains("conversion is not supported at this position yet"),
                            );
                            if internal(&diagnostic.message) {
                                failures.push(format!("boundary lsp {key}: {diagnostic:?}"));
                            }
                        }
                    }
                    if count != 1 {
                        failures.push(format!(
                            "boundary lsp {key}: expected one placeholder, got {count}"
                        ));
                    }
                    cells += 1;
                }
                for arguments in [
                    vec!["destroy", "--auto-approve", "."],
                    vec!["state", "refresh", "."],
                ] {
                    let result = command(directory.path(), &arguments);
                    let text = output(&result);
                    if text
                        .matches("conversion is not supported at this position yet")
                        .count()
                        != 1
                        || internal(&text)
                    {
                        failures.push(format!("boundary {arguments:?} {key}: {text}"));
                    }
                    cells += 1;
                }
                if matches!(liveness, Liveness::ReadOnly) {
                    excluded += 1;
                } else {
                    let project = ResolvedPath::new(directory.path()).unwrap();
                    let context = carina_core::parser::ProviderContext::new(
                        carina_core::EnvironmentSnapshot::empty(),
                    );
                    let result = carina_cli::commands::apply::run_apply(
                        &runtime,
                        &project,
                        directory.path(),
                        true,
                        false,
                        std::num::NonZeroUsize::new(2).unwrap(),
                        false,
                        &context,
                        carina_core::shutdown::ShutdownToken::running(),
                    )
                    .await;
                    let text = result
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default();
                    if text
                        .matches("conversion is not supported at this position yet")
                        .count()
                        != 1
                        || internal(&text)
                    {
                        failures.push(format!("boundary apply {key}: {text}"));
                    }
                    cells += 1;
                }
            }
        }
    }
    eprintln!(
        "boundary totality cells: {cells}; excluded {excluded} (of {})",
        10 * 13 * 2 * 6
    );
    assert_eq!(cells + excluded, 10 * 13 * 2 * 6);
    assert_eq!(
        cells, 544,
        "read-only scopes cannot be validate, plan, apply or LSP runs"
    );
    assert_eq!(excluded, 1016);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
        let mut generated = fixture(Position::Root, Liveness::Live, Quality::Valid, converted);
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
    let mut generated = fixture(Position::Root, Liveness::Live, Quality::Valid, false);
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
fn merge_module_require_failure_stops_before_resource_conversion_admission() {
    let directory = tempfile::tempdir().unwrap();
    for passes in [false, true] {
        let condition = if passes { "true" } else { "false" };
        require_validation_fixture(true, condition, "mock.Sink { attr = 42 as mock.Name }")
            .write(directory.path());
        let cli = validate(directory.path());
        assert_eq!(cli["status"], "error", "{cli}");
        assert!(!internal(&cli.to_string()), "{cli}");
        let errors = cli["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{cli}");
        if passes {
            assert_eq!(conversion_count(&cli), 1, "{cli}");
            assert_eq!(errors[0]["code"], "conversion_scalar_kind", "{cli}");
            assert!(
                errors[0]["location"]["file"]
                    .as_str()
                    .unwrap()
                    .ends_with("child/main.crn"),
                "{cli}"
            );
        } else {
            assert_eq!(conversion_count(&cli), 0, "{cli}");
            assert!(errors[0]["code"].is_null(), "{cli}");
            assert_eq!(
                errors[0]["message"],
                "module 'child' (call 'net'): arguments : require merge guard",
                "{cli}"
            );
            require_validation_fixture(true, condition, "mock.Sink { attr = 'ok' }")
                .write(directory.path());
            let control = validate(directory.path());
            assert_eq!(
                cli, control,
                "ordinary require failure is exactly main's output with a valid no-as resource"
            );
        }
    }
}
