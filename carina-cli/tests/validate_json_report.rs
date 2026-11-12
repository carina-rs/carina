//! Validate has one report for both transports, including failed validation.
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use carina_core::provider::ProviderFactory;
use carina_core::schema::SchemaRegistry;
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use serde_json::Value;
use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

fn validate(path: &Path, json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .arg("validate")
        .arg(path)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    if json {
        command.arg("--json");
    }
    command.output().unwrap()
}

fn report(output: &Output, errors: usize, warnings: usize) -> Value {
    assert_eq!(
        output.status.code(),
        Some(if errors == 0 { 0 } else { 1 }),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // from_slice rejects trailing documents and non-JSON notices.
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["status"], if errors == 0 { "ok" } else { "error" });
    for (field, count) in [("errors", errors), ("warnings", warnings)] {
        if count == 0 {
            assert!(json.get(field).is_none(), "{json}");
        } else {
            assert_eq!(json[field].as_array().unwrap().len(), count, "{json}");
        }
    }
    if errors > 0 {
        assert_eq!(json["resource_count"], 0);
        assert_eq!(json["resources"], serde_json::json!([]));
    }
    json
}

fn round_three_diagnostics(source: &str, path: &Path) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), vec!["mock".into()]);
    let document = Document::new_with_path(
        source.to_string(),
        path.to_path_buf(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine.analyze(&document, Some(path))
}

#[test]
fn round13_protected_ast_evaluation_is_content_free_on_all_surfaces() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n").unwrap();
    for source in [
        "let hidden = { private = 1 }['CANARY'] |> secret()",
        "let hidden = secret({ private = 1 }['CANARY'])",
        "let hidden = secret(length({ private = 1 }['CANARY']))",
        "let hidden = secret(\"${{ private = 1 }['CANARY']}\")",
        "let hidden = secret(<<EOF\n${{ private = 1 }['CANARY']}\nEOF\n)",
        "let hidden = 'payload' |> CANARY() |> secret() |> length()",
        "let hidden = { CANARY = 1 } + 1 |> secret() ?? 1",
    ] {
        std::fs::write(&path, source).unwrap();
        let human = validate(fixture.path(), false);
        let json = validate(fixture.path(), true);
        let diagnostics = round_three_diagnostics(source, &path);
        let error = carina_core::parser::parse_file(
            source,
            &path,
            &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        )
        .unwrap_err();
        let surfaces = format!(
            "human stdout: {}\nhuman stderr: {}\nJSON stdout: {}\nJSON stderr: {}\nLSP: {diagnostics:?}\nParseError: {error:?}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr),
            String::from_utf8_lossy(&json.stdout),
            String::from_utf8_lossy(&json.stderr),
        );
        assert_eq!(human.status.code(), Some(1), "{surfaces}");
        assert_eq!(json.status.code(), Some(1), "{surfaces}");
        assert!(!surfaces.contains("CANARY"), "{surfaces}");
        assert!(!surfaces.contains("private"), "{surfaces}");
        assert!(json.stderr.is_empty(), "{surfaces}");
        let report: Value = serde_json::from_slice(&json.stdout).unwrap();
        assert!(
            report["errors"][0]["location"]["range"]["start"]["line"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            report["errors"][0]["location"]["range"]["start"]["column"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            String::from_utf8_lossy(&human.stderr).contains("line 1, column"),
            "{surfaces}"
        );
        assert!(
            diagnostics.iter().any(
                |finding| finding.severity == Some(DiagnosticSeverity::ERROR)
                    && finding.range.start.character > 0
            ),
            "{surfaces}"
        );
    }
}

#[test]
fn round13_public_evaluation_error_keeps_human_json_and_lsp_text() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let message = "key 'CANARY' not found in map; available keys: 'private'";
    let source = "let visible = { private = 1 }['CANARY']";
    std::fs::write(&path, source).unwrap();
    let human = validate(fixture.path(), false);
    assert_eq!(human.status.code(), Some(1));
    assert!(human.stdout.is_empty());
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        format!(
            "Error: {}: Invalid expression at line 1: {message}\n",
            path.display()
        )
    );
    let json = report(&validate(fixture.path(), true), 1, 0);
    assert_eq!(json["errors"][0]["message"], message);
    assert!(json["errors"][0]["location"]["range"].is_null());
    let diagnostics = round_three_diagnostics(source, &path);
    assert!(
        diagnostics.iter().any(|finding| finding.message == message),
        "{diagnostics:?}"
    );
}

#[test]
fn round13_deferred_secret_evaluation_stays_safe_on_all_surfaces() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n").unwrap();
    for source in [
        "fn pick(input) { secret(input['CANARY']) }\nlet hidden = pick({ private = 1 })",
        "fn pick(input) { input['CANARY'] |> secret() }\nlet hidden = pick({ private = 1 })",
        "fn pick(input) { input['CANARY'] }\nlet hidden = secret(pick({ private = 1 }))",
    ] {
        std::fs::write(&path, source).unwrap();
        let human = validate(fixture.path(), false);
        let json = validate(fixture.path(), true);
        let diagnostics = round_three_diagnostics(source, &path);
        let surfaces = format!(
            "{}{}{}{} {diagnostics:?}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr),
            String::from_utf8_lossy(&json.stdout),
            String::from_utf8_lossy(&json.stderr)
        );
        assert!(
            !surfaces.contains("CANARY") && !surfaces.contains("private"),
            "{surfaces}"
        );
        assert_eq!(human.status.code(), Some(1), "{surfaces}");
        assert_eq!(json.status.code(), Some(1), "{surfaces}");
        assert!(json.stderr.is_empty());
        assert!(
            diagnostics
                .iter()
                .any(|finding| finding.severity == Some(DiagnosticSeverity::ERROR)),
            "{surfaces}"
        );
    }
}

#[test]
fn round13_module_argument_secret_evaluation_stays_safe_on_all_surfaces() {
    let fixture = tempfile::tempdir().unwrap();
    let child = fixture.path().join("child");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let root_source =
        "let child = use { source = './child' }\nchild { input = { private = 'value' } }\n";
    let child_source = "arguments { input: map(String) }\nlet hidden = input['CANARY'] |> secret()\nmock.test.resource { name = 'safe' tags = hidden }\n";
    let path = fixture.path().join("main.crn");
    let child_path = child.join("main.crn");
    std::fs::write(&path, root_source).unwrap();
    std::fs::write(&child_path, child_source).unwrap();
    let human = validate(fixture.path(), false);
    let json = validate(fixture.path(), true);
    let mut diagnostics = round_three_diagnostics(root_source, &path);
    diagnostics.extend(round_three_diagnostics(child_source, &child_path));
    let surfaces = format!(
        "{}{}{}{} {diagnostics:?}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr),
        String::from_utf8_lossy(&json.stdout),
        String::from_utf8_lossy(&json.stderr)
    );
    assert!(
        !surfaces.contains("CANARY") && !surfaces.contains("private"),
        "{surfaces}"
    );
    assert_eq!(human.status.code(), Some(1), "{surfaces}");
    assert_eq!(json.status.code(), Some(1), "{surfaces}");
    assert!(json.stderr.is_empty());
    assert!(
        diagnostics
            .iter()
            .any(|finding| finding.severity == Some(DiagnosticSeverity::ERROR)),
        "{surfaces}"
    );
}

#[test]
fn failed_public_identifiers_keep_public_syntax_excerpts() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n").unwrap();
    for name in ["ifsecret_name", "secret_name"] {
        let source = format!("let x = {name}('PUBLIC') ???");
        std::fs::write(&path, &source).unwrap();
        let human = validate(fixture.path(), false);
        assert_eq!(human.status.code(), Some(1));
        let stderr = String::from_utf8(human.stderr).unwrap();
        assert!(stderr.contains(&source), "{stderr}");
        assert!(!stderr.contains("<redacted>"), "{stderr}");
        let json = report(&validate(fixture.path(), true), 1, 0);
        assert!(
            json["errors"][0]["message"]
                .as_str()
                .unwrap()
                .starts_with("Syntax error:")
        );
        let diagnostics = round_three_diagnostics(&source, &path);
        let syntax = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .unwrap_or_else(|| panic!("{diagnostics:?}"));
        assert!(syntax.message.contains(&source), "{syntax:?}");
        assert!(!syntax.message.contains("<redacted>"), "{syntax:?}");
    }
}

#[test]
fn round8_deferred_headers_keep_parsed_context_on_all_surfaces() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    for (root, binding, iterable, expected) in [
        ("cert", "secret", "cert.list", "for secret in cert.list"),
        ("cert", "item", "cert.list", "for item in cert.list"),
        (
            "cert",
            "item",
            "secret(cert.list)",
            "for item in secret(<redacted>)",
        ),
        ("secret", "item", "secret.list", "for item in secret.list"),
    ] {
        let source = format!(
            "let {root} = mock.example.consumer {{ name = 'cert' }}\n\
             let records = for {binding} in {iterable} {{ read mock.iam.Roles {{ name_regex = {binding} }} }}\n"
        );
        std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(&path, &source).unwrap();
        let message = format!(
            "Deferred `for` expression `{expected}` at line 2 cannot use read `mock.iam.Roles`: its iterable \
             is still unresolved after module expansion, and only a managed resource body can be \
             deferred until apply. Iterate over a value known before module expansion, or put \
             managed resources directly in the loop body"
        );
        let human = validate(fixture.path(), false);
        assert_eq!(human.status.code(), Some(1));
        assert_eq!(human.stdout, b"Validating...\n");
        assert_eq!(
            String::from_utf8(human.stderr).unwrap(),
            format!(
                "Error: Module resolution error: Parse error in main.crn: main.crn:2:15: {message}\n"
            )
        );
        let json = report(&validate(fixture.path(), true), 1, 0);
        assert_eq!(json["errors"][0]["message"], message);
        let diagnostics = round_three_diagnostics(&source, &path);
        let lsp_message = format!("Parse error in main.crn: main.crn:2:15: {message}");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == lsp_message),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn round7_configuration_key_diagnostics_remain_typed_on_all_surfaces() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = r#"backend local { "${secret('CANARY')}" = secret('VALUE_CANARY') }"#;
    std::fs::write(&path, source).unwrap();
    let human = validate(fixture.path(), false);
    assert_eq!(human.status.code(), Some(1));
    let json_output = validate(fixture.path(), true);
    let json = report(&json_output, 1, 0);
    let diagnostics = round_three_diagnostics(source, &path);
    let all = format!(
        "{}{}{} {diagnostics:?}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr),
        String::from_utf8_lossy(&json_output.stdout)
    );
    assert!(!all.contains("CANARY"), "{all}");
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("secret(<redacted>)"), "{message}");
    assert!(
        diagnostics
            .iter()
            .any(|finding| finding.message.contains("secret(<redacted>)"))
    );
}

#[test]
fn round7_many_failed_file_lint_positions_have_a_bounded_projection() {
    let source = format!(
        "???\n{}",
        (0..4000)
            .map(|index| format!("let Bad{index} = 1\n"))
            .collect::<String>()
    );
    let findings = round_three_diagnostics(&source, Path::new("main.crn"));
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.message.starts_with("Binding 'Bad"))
            .count(),
        4000
    );
    let last = findings
        .iter()
        .find(|finding| finding.message.starts_with("Binding 'Bad3999'"))
        .unwrap();
    assert_eq!(
        last.range.start,
        tower_lsp::lsp_types::Position::new(4000, 4)
    );
    assert_eq!(last.range.end.character, 11);
}

#[test]
fn computed_reference_mismatches_keep_the_direct_leaf_origin_policy() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    for expression in [
        "src.max_session_duration ?? 1",
        "src.max_session_duration + 1",
    ] {
        let source = format!(
            "provider mock {{}}\nlet src = mock.iam.Role {{ role_name = 'src' }}\nmock.test.resource {{ name = {expression} }}\n"
        );
        std::fs::write(&path, &source).unwrap();
        let json = report(&validate(fixture.path(), true), 1, 0);
        let message = json["errors"][0]["message"].as_str().unwrap();
        assert!(!message.contains("(from "), "{json}");
        let human = validate(fixture.path(), false);
        let stderr = String::from_utf8(human.stderr).unwrap();
        if expression.contains("??") {
            assert!(stderr.contains("cannot assign Int to 'name': expected String, got Int (from src.max_session_duration)"), "{stderr}");
        } else {
            assert!(stderr.contains("expected String, got Int"), "{stderr}");
            assert!(!stderr.contains("(from "), "{stderr}");
        }
        let diagnostics = round_three_diagnostics(&source, &path);
        assert!(
            diagnostics.iter().any(|d| d.message == message),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn public_heredoc_marker_errors_keep_their_original_text() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "let visible = <<PUBLIC_KEY\nvalue\n";
    std::fs::write(&path, source).unwrap();
    let message = "Unterminated heredoc at line 3: closing marker 'PUBLIC_KEY' not found";
    let human = validate(fixture.path(), false);
    assert_eq!(human.status.code(), Some(1));
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert!(stderr.contains(message), "{stderr}");
    let output = validate(fixture.path(), true);
    let json = report(&output, 1, 0);
    assert_eq!(json["errors"][0]["message"], message);
    let diagnostics = round_three_diagnostics(source, &path);
    assert!(
        diagnostics.iter().any(|d| d.message.contains(message)),
        "{diagnostics:?}"
    );
}

#[test]
fn constraint_secret_duplicate_is_redacted_in_failed_json_and_lsp() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "require secret(x[{ 'private-key' = 1 'private-key' = 2 }]), 'no'\n";
    std::fs::write(&path, source).unwrap();
    let output = validate(fixture.path(), true);
    let json = report(&output, 1, 1);
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("private-key")
    );
    assert!(
        json["warnings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("'<redacted>'")
    );
    let column = source.find("secret").unwrap() + 1;
    assert_eq!(
        json["warnings"][0]["location"]["range"],
        serde_json::json!({"start":{"line":1,"column":column}, "end":{"line":1,"column":column+6}})
    );
    let diagnostics = round_three_diagnostics(source, &path);
    assert!(!format!("{diagnostics:?}").contains("private-key"));
    let warning = diagnostics
        .iter()
        .find(|d| d.message.starts_with("Duplicate attribute"))
        .unwrap();
    assert!(warning.message.contains("'<redacted>'"));
    assert_eq!(warning.range.start.character as usize, column - 1);
    assert_eq!(warning.range.end.character as usize, column + 5);
    let human = validate(fixture.path(), false);
    assert!(
        !String::from_utf8(human.stdout)
            .unwrap()
            .contains("Duplicate attribute")
    );
    assert!(
        !String::from_utf8(human.stderr)
            .unwrap()
            .contains("private-key")
    );
}

#[test]
fn nested_reference_context_matches_human_json_and_lsp() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "provider mock {}\nlet src = mock.iam.Role { role_name = 'src' }\nmock.test.resource { name = 'dst' tags = { public = src.max_session_duration } }\n";
    std::fs::write(&path, source).unwrap();
    let json = report(&validate(fixture.path(), true), 1, 0);
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("cannot assign Int to 'tags': "),
        "{json}"
    );
    assert!(
        message.ends_with("(from src.max_session_duration)"),
        "{json}"
    );
    let human = validate(fixture.path(), false);
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert!(stderr.contains(message), "{stderr}");
    let diagnostics = round_three_diagnostics(source, &path);
    assert!(
        diagnostics.iter().any(|d| d.message == message),
        "{diagnostics:?}"
    );
}

#[test]
fn unused_bindings_survive_unrelated_validation_failures() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("a.crn");
    let source = "provider mock {}\nlet unused = mock.test.resource { name = 'ok' }\n";
    std::fs::write(&path, source).unwrap();
    std::fs::write(
        fixture.path().join("b.crn"),
        "mock.test.resource { name = true }\n",
    )
    .unwrap();
    let json = report(&validate(fixture.path(), true), 1, 1);
    assert_eq!(json["warnings"][0]["type"], "unused_binding");
    assert_eq!(
        json["warnings"][0]["message"],
        "Unused let binding 'unused'"
    );
    assert_eq!(
        json["warnings"][0]["location"]["file"],
        path.display().to_string()
    );
    let diagnostics = round_three_diagnostics(source, &path);
    assert!(
        diagnostics.iter().any(|d| d.message
            == "Unused let binding 'unused'. Consider using an anonymous resource instead."),
        "{diagnostics:?}"
    );
    let human = validate(fixture.path(), false);
    assert_eq!(human.status.code(), Some(1));
    assert!(
        !String::from_utf8(human.stdout)
            .unwrap()
            .contains("Unused let binding")
    );
    assert!(
        !String::from_utf8(human.stderr)
            .unwrap()
            .contains("Unused let binding")
    );
}

#[test]
fn public_tag_style_ignores_secret_population_on_human_and_lsp() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "provider mock {}\nmock.test.resource {\n name = 'dst'\n tags = {\n Public = 'x'\n one = 'x'\n two = 'x'\n }\n}\nlet hidden = secret({\n tags = {\n A = 'x'\n B = 'x'\n C = 'x'\n }\n})\n";
    std::fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .arg("lint")
        .arg(fixture.path())
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let message = "Tag key 'Public' doesn't match the dominant style (snake_case). Use consistent casing for tag keys.";
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("warning: "))
            .collect::<Vec<_>>(),
        [format!("warning: {}:5  {message}", path.display())]
    );
    let diagnostics = round_three_diagnostics(source, &path);
    let tag_warnings = diagnostics
        .iter()
        .filter(|d| d.message.starts_with("Tag key '"))
        .collect::<Vec<_>>();
    assert_eq!(tag_warnings.len(), 1, "{diagnostics:?}");
    assert_eq!(tag_warnings[0].message, message);
    assert_eq!(tag_warnings[0].range.start.line, 4);
    for key in ["A", "B", "C"] {
        assert!(!stderr.contains(&format!("Tag key '{key}'")));
        assert!(
            diagnostics
                .iter()
                .all(|d| !d.message.contains(&format!("Tag key '{key}'")))
        );
    }
}

#[test]
fn multifile_duplicate_key_redaction_matches_human_json_and_lsp() {
    let fixture = tempfile::tempdir().unwrap();
    let context_path = fixture.path().join("main.crn");
    let bindings_path = fixture.path().join("bindings.crn");
    let providers_path = fixture.path().join("providers.crn");
    let providers = "provider mock {}\n";
    let context = "mock.test.resource { name = 'redaction-control' tags = hidden }\n";
    std::fs::write(&context_path, context).unwrap();
    std::fs::write(&providers_path, providers).unwrap();
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), vec!["mock".into()]);

    for (source, displayed_key, token) in [
        (
            "let hidden = { 'public-key' = 'a' 'public-key' = 'b' }\n",
            "public-key",
            "'public-key'",
        ),
        (
            "let hidden = secret({ 'private-key' = 'a' 'private-key' = 'b' })\n",
            "<redacted>",
            "secret",
        ),
    ] {
        std::fs::write(&bindings_path, source).unwrap();
        let column = source[..source.rfind(token).unwrap()].chars().count() + 1;
        let end_column = column + token.chars().count();
        let legacy_message = format!(
            "Duplicate attribute '{displayed_key}' at line 1 (first defined on line 1). The last value will be used."
        );
        let lsp_message = format!(
            "Duplicate attribute '{displayed_key}' (first defined on line 1). The last value will be used."
        );

        let output = validate(fixture.path(), true);
        assert!(
            !std::str::from_utf8(&output.stdout)
                .unwrap()
                .contains("private-key")
        );
        let json = report(&output, 0, 1);
        assert_eq!(json["resource_count"], 1);
        let warning = &json["warnings"][0];
        assert_eq!(warning["type"], "duplicate_attribute");
        assert_eq!(warning["severity"], "warning");
        assert_eq!(warning["message"], legacy_message);
        assert_eq!(
            warning["location"]["file"],
            bindings_path.display().to_string()
        );
        assert_eq!(
            warning["location"]["range"],
            serde_json::json!({
                "start": { "line": 1, "column": column },
                "end": { "line": 1, "column": end_column }
            })
        );

        let mut duplicate_diagnostics = Vec::new();
        for (path, text) in [
            (&context_path, context),
            (&providers_path, providers),
            (&bindings_path, source),
        ] {
            let document = Document::new_with_path(
                text.to_string(),
                path.clone(),
                Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            for diagnostic in engine.analyze(&document, Some(path)) {
                assert_ne!(
                    diagnostic.severity,
                    Some(DiagnosticSeverity::ERROR),
                    "{diagnostic:?}"
                );
                assert!(
                    !diagnostic.message.contains("private-key"),
                    "{diagnostic:?}"
                );
                assert!(
                    diagnostic.message.starts_with("Duplicate attribute '"),
                    "{diagnostic:?}"
                );
                assert_eq!(path, &bindings_path);
                duplicate_diagnostics.push(diagnostic);
            }
        }
        assert_eq!(duplicate_diagnostics.len(), 1);
        let diagnostic = &duplicate_diagnostics[0];
        assert_eq!(diagnostic.message, lsp_message);
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diagnostic.range.start.line, 0);
        assert_eq!(diagnostic.range.end.line, 0);
        assert_eq!(diagnostic.range.start.character as usize, column - 1);
        assert_eq!(diagnostic.range.end.character as usize, end_column - 1);

        let output = validate(fixture.path(), false);
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(stderr.is_empty(), "{stderr}");
        assert!(!stdout.contains("private-key"));
        assert!(!stderr.contains("private-key"));
        let warnings: Vec<_> = stdout
            .lines()
            .filter(|line| line.starts_with("⚠ "))
            .collect();
        assert_eq!(
            warnings,
            vec![format!("⚠ {}:{legacy_message}", bindings_path.display())],
            "stdout: {stdout:?}; stderr: {stderr:?}"
        );
    }
}

#[test]
fn duplicate_secret_keys_are_redacted_and_anchored_outside_the_secret() {
    assert_secret_duplicate("let hidden = secret({ 'private-key' = 'a' 'private-key' = 'b' })\n");
}

#[test]
fn piped_secret_keys_are_redacted_and_anchored_outside_the_secret() {
    assert_secret_duplicate(
        "let hidden = { 'private-key' = 'a' 'private-key' = 'b' } |> secret()\n",
    );
}

fn assert_secret_duplicate(source: &str) {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), source).unwrap();
    let json = report(&validate(fixture.path(), true), 0, 1);
    let warning = &json["warnings"][0];
    assert!(!json.to_string().contains("private-key"));
    let column = source.find("secret").unwrap() + 1;
    assert_eq!(
        warning["location"]["range"],
        serde_json::json!({
            "start": { "line": 1, "column": column },
            "end": { "line": 1, "column": column + 6 }
        })
    );
    let human = validate(fixture.path(), false);
    assert!(human.status.success());
    assert!(!String::from_utf8_lossy(&human.stdout).contains("private-key"));
    assert!(human.stderr.is_empty());
    let document = Document::new_with_path(
        source.to_string(),
        fixture.path().join("main.crn"),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(Vec::new()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), Vec::new());
    let lint = engine
        .analyze(&document, Some(&fixture.path().join("main.crn")))
        .into_iter()
        .find(|finding| finding.message.contains("Duplicate attribute"))
        .unwrap();
    assert!(!lint.message.contains("private-key"));
    assert_eq!(lint.range.start.character as usize + 1, column);
    assert_eq!(lint.range.end.character as usize + 1, column + 6);
}

#[test]
fn unused_root_binding_does_not_borrow_a_used_module_bindings_location() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(fixture.path().join("main.crn"), "provider mock {}\nlet temp = mock.test.resource { name = 'root' }\nlet child = use { source = './child' }\nchild {}\n").unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "let temp = mock.test.resource { name = 'child' }\nattributes { name = temp.name }\n",
    )
    .unwrap();
    let json = report(&validate(fixture.path(), true), 0, 1);
    assert_eq!(json["warnings"][0]["message"], "Unused let binding 'temp'");
    assert_eq!(
        json["warnings"][0]["location"]["file"],
        fixture.path().join("main.crn").display().to_string()
    );
    let human = validate(fixture.path(), false);
    assert_eq!(
        String::from_utf8_lossy(&human.stdout)
            .matches("Unused let binding 'temp'")
            .count(),
        1
    );
}

#[test]
fn relative_project_module_locations_are_complete_source_labels() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fixture.path().join("infra/child")).unwrap();
    std::fs::write(
        fixture.path().join("infra/main.crn"),
        "let child = use { source = './child' }\nchild {}\n",
    )
    .unwrap();
    std::fs::write(fixture.path().join("infra/child/main.crn"), "let = {\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "infra", "--json"])
        .current_dir(fixture.path())
        .output()
        .unwrap();
    let json = report(&output, 1, 0);
    assert_eq!(
        json["errors"][0]["location"]["file"],
        "infra/child/main.crn"
    );
}

#[test]
fn independent_duplicate_lints_survive_early_and_late_failures_only_in_json() {
    for failure in [
        "mock.test.resource { name = true }\n",
        "let child = use { source = './missing' }\nchild {}\n",
        "let state = upstream_state { source = './missing' }\n",
        "let = {\n",
    ] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            "provider mock {}\nmock.test.resource { name = 'one' name = 'two' }\n",
        )
        .unwrap();
        std::fs::write(fixture.path().join("failure.crn"), failure).unwrap();
        let output = validate(fixture.path(), true);
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty());
        assert_eq!(json["warnings"].as_array().map(Vec::len), Some(1), "{json}");
        assert_eq!(json["warnings"][0]["type"], "duplicate_attribute");
        let human = validate(fixture.path(), false);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr)
        );
        assert!(!text.contains("Duplicate attribute"), "{text}");
    }
}

#[test]
fn resource_inventory_helper_runs_the_production_upstream_preflight() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "let state = upstream_state { source = './missing' }\n",
    )
    .unwrap();
    let panic = std::panic::catch_unwind(|| {
        carina_cli::commands::validate::validated_resource_ids_with_factories(
            &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
            Vec::new(),
        )
    })
    .expect_err("the production collector must reject the missing upstream");
    let text = panic.downcast_ref::<String>().unwrap();
    assert!(
        text.contains("upstream_state 'state': source './missing' does not exist"),
        "{text}"
    );
}

#[test]
fn local_trust_and_unwritable_cache_notices_belong_to_the_report() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let wasm = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("the mock WASM fixture must be built");
    for trusted in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let kind = if trusted { "aws" } else { "mock" };
        let source = fixture.path().join("local.wasm");
        std::fs::copy(&wasm, &source).unwrap();
        let installed = fixture
            .path()
            .join(".carina/providers/file/local/local.wasm");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::copy(&source, &installed).unwrap();
        std::fs::create_dir_all(home.path().join(".carina")).unwrap();
        std::fs::write(home.path().join(".carina/cache"), "cache is a file").unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            format!("provider {kind} {{ source = 'file://local.wasm' }}\n"),
        )
        .unwrap();
        let run = |json| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .current_dir(fixture.path())
                .args(["validate", "."])
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("CARINA_TEST_HOME", home.path())
                .env(
                    "CARINA_PLUGIN_CACHE_DIR",
                    home.path().join(".carina/plugin-cache"),
                )
                .env("CARINA_TRUST_RECORD_DIR", home.path().join(".carina/trust"))
                .env_remove("CARINA_TRUST_LOCAL_PROVIDER")
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE");
            if trusted {
                command
                    .arg("--trust-local-provider")
                    .arg(format!("{}=aws", source.display()));
            }
            if json {
                command.arg("--json");
            }
            command.output().unwrap()
        };
        let human = run(false);
        let stderr = String::from_utf8(human.stderr).unwrap();
        assert!(
            stderr.contains("Failed to write precompile cache, loading directly:"),
            "{stderr}"
        );
        assert_eq!(
            stderr.contains("carina: trusting local provider"),
            trusted,
            "{stderr}"
        );
        let json = run(true);
        assert_eq!(json.status, human.status);
        report(&json, if trusted { 2 } else { 0 }, 0);
    }
}

#[test]
fn legacy_lock_migration_notices_belong_to_the_report() {
    for remote in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            if remote {
                "provider broken { source = 'github.com/example/carina-provider-broken' }\n"
            } else {
                "provider mock {}\n"
            },
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("carina-providers.lock"),
            "version = 1\nprovider = []\n",
        )
        .unwrap();
        let human = validate(fixture.path(), false);
        let text = String::from_utf8(human.stderr).unwrap();
        assert_eq!(
            text.matches("Provider lock format migration:").count(),
            if remote { 2 } else { 1 },
            "{text}"
        );
        let output = validate(fixture.path(), true);
        assert_eq!(output.status, human.status);
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["status"], if remote { "error" } else { "ok" });
        assert!(!json.to_string().contains("Provider lock format migration"));
    }
}

#[test]
fn malformed_lock_diagnostics_never_serialize_source_snippets_or_values() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        fixture.path().join("carina-providers.lock"),
        "'private-lock-key' = 'private-lock-value' ???\n",
    )
    .unwrap();
    let output = validate(fixture.path(), true);
    let json = report(&output, 1, 0);
    assert!(!json.to_string().contains("private-lock-key"));
    assert!(!json.to_string().contains("private-lock-value"));
    assert_eq!(
        json["errors"][0]["location"]["file"],
        fixture
            .path()
            .join("carina-providers.lock")
            .display()
            .to_string()
    );
    assert!(json["errors"][0]["location"]["range"].is_null());
}

#[test]
fn multifile_input_findings_survive_unrelated_failures_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().unwrap();
    for (file, source) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        ),
        ("exports.crn", "exports { name = tagged.name }\n"),
    ] {
        std::fs::write(fixture.path().join(file), source).unwrap();
    }
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let providers = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);
    for (reserved, unrelated, errors) in [
        (false, false, 0),
        (true, false, 1),
        (false, true, 1),
        (true, true, 2),
    ] {
        let tags = if reserved {
            "  tags = { 'aWs:private-key' = 'secret-tag-value' }\n"
        } else {
            ""
        };
        let bad = if unrelated {
            "mock.test.resource { name = true }\n"
        } else {
            ""
        };
        let source = format!(
            "// Unicode before authored positions: 🦀日本語\nlet tagged = mock.test.input_constrained {{\n  name = '192.168.1.1'\n{tags}}}\n{bad}"
        );
        std::fs::write(fixture.path().join("main.crn"), &source).unwrap();
        let output = validate(fixture.path(), true);
        let json = report(&output, errors, 1);
        assert!(
            json["warnings"][0]["message"]
                .as_str()
                .unwrap()
                .contains("must not have IPv4 dotted-quad form")
        );
        let human = validate(fixture.path(), false);
        assert_eq!(human.status.code(), output.status.code());
        let human = format!(
            "{}{}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr)
        );
        let mut lsp = Vec::new();
        for file in ["providers.crn", "backend.crn", "exports.crn", "main.crn"] {
            let path = fixture.path().join(file);
            let text = std::fs::read_to_string(&path).unwrap();
            let document = Document::new_with_path(
                text,
                path.clone(),
                Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            lsp.extend(
                engine
                    .analyze(&document, Some(&path))
                    .into_iter()
                    .map(|finding| (file, finding)),
            );
        }
        let findings = json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .chain(json["errors"].as_array().into_iter().flatten())
            .collect::<Vec<_>>();
        assert_eq!(lsp.len(), findings.len(), "{lsp:#?}\n{json}");
        for finding in findings {
            let message = finding["message"].as_str().unwrap();
            assert!(human.contains(message), "missing {message:?}: {human}");
            let (file, diagnostic) = lsp
                .iter()
                .find(|(_, diagnostic)| diagnostic.message == message)
                .unwrap_or_else(|| panic!("missing {message:?}: {lsp:#?}"));
            let severity = if diagnostic.severity == Some(DiagnosticSeverity::WARNING) {
                "warning"
            } else {
                "error"
            };
            if message.contains("mock.name.not_ipv4")
                || message.contains("mock.tags.reserved_prefix")
            {
                assert_eq!(finding["severity"], severity);
            } else {
                // Preserve the existing editor warning / CLI failure policy
                // for unrelated resource attribute type mismatches.
                assert_eq!(finding["severity"], "error");
                assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
            }
            assert!(
                finding["location"]["file"]
                    .as_str()
                    .unwrap()
                    .ends_with(file)
            );
            let text = std::fs::read_to_string(fixture.path().join(file)).unwrap();
            let scalar_column = |line: u32, utf16: u32| {
                let mut units = 0;
                text.lines()
                    .nth(line as usize)
                    .unwrap()
                    .chars()
                    .take_while(|c| {
                        units += c.len_utf16();
                        units <= utf16 as usize
                    })
                    .count()
                    + 1
            };
            let range = diagnostic.range;
            assert_eq!(
                finding["location"]["range"],
                serde_json::json!({
                    "start": {"line": range.start.line + 1, "column": scalar_column(range.start.line, range.start.character)},
                    "end": {"line": range.end.line + 1, "column": scalar_column(range.end.line, range.end.character)}
                }),
                "{finding}"
            );
            if message.contains("mock.name.not_ipv4")
                || message.contains("mock.tags.reserved_prefix")
            {
                let code = if message.contains("mock.name.not_ipv4") {
                    "mock.name.not_ipv4"
                } else {
                    "mock.tags.reserved_prefix"
                };
                assert_eq!(finding["code"], code);
                assert_eq!(diagnostic.code, Some(NumberOrString::String(code.into())));
                assert_eq!(finding["type"], "input_rule");
            }
        }
        for secret in ["aWs:private-key", "secret-tag-value", "192.168.1.1"] {
            assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
            assert!(!human.contains(secret));
            assert!(!format!("{lsp:?}").contains(secret));
        }
    }
}

#[test]
fn failure_matrix_always_emits_one_document() {
    for (files, needle) in [
        (vec![], "No .crn files"),
        (vec![("main.crn", "let = {")], ""),
        (
            vec![
                ("main.crn", "backend local {}\n"),
                ("other.crn", "backend local {}\n"),
            ],
            "multiple backend",
        ),
        (
            vec![(
                "main.crn",
                "provider broken { source = 'file:///definitely-missing/broken.wasm' }\n",
            )],
            "Provider 'broken'",
        ),
        (
            vec![(
                "main.crn",
                "provider mock {}\nmock.test.resource { name = true }\n",
            )],
            "",
        ),
        (
            vec![(
                "main.crn",
                "let child = use { source = './missing-module' }\nchild {}\n",
            )],
            "",
        ),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(fixture.path().join(name), text).unwrap();
        }
        let output = validate(fixture.path(), true);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["resources"], serde_json::json!([]));
        assert_eq!(json["resource_count"], 0);
        assert!(!json["errors"].as_array().unwrap().is_empty());
        assert!(json["errors"].to_string().contains(needle), "{json}");
        for finding in json["errors"].as_array().unwrap() {
            assert_eq!(finding["severity"], "error");
            assert!(finding.get("code").is_some());
            assert!(finding.get("location").is_some());
        }
    }
    let fixture = tempfile::tempdir().unwrap();
    let missing = fixture.path().join("missing");
    report(&validate(&missing, true), 1, 0);
    std::fs::write(fixture.path().join("file.crn"), "").unwrap();
    report(&validate(&fixture.path().join("file.crn"), true), 1, 0);
}

#[test]
fn runtime_setup_failure_is_json_before_configuration_loading() {
    let fixture = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "--json"])
        .current_dir(fixture.path())
        .env("CARINA_TRUST_RECORD_DIR", "relative-trust-directory")
        .output()
        .unwrap();
    let json = report(&output, 1, 0);
    assert!(
        json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("CARINA_TRUST_RECORD_DIR must be an absolute path")
    );
    assert!(json["errors"][0]["code"].is_null());
    assert!(json["errors"][0]["location"].is_null());
}

#[test]
fn parser_and_lint_warnings_have_scalar_ranges_and_legacy_fields() {
    let fixture = tempfile::tempdir().unwrap();
    let source = "provider mock {}\nlet temp = mock.test.resource { comment = '🦀日本語' name = 'one' name = 'two' }\n";
    std::fs::write(fixture.path().join("main.crn"), source).unwrap();
    let output = validate(fixture.path(), true);
    let json = report(&output, 0, 2);
    let warnings = json["warnings"].as_array().unwrap();
    let unused = warnings
        .iter()
        .find(|warning| warning["type"] == "unused_binding")
        .unwrap();
    assert_eq!(unused["message"], "Unused let binding 'temp'");
    assert!(unused.get("file").is_none());
    let duplicate = warnings
        .iter()
        .find(|warning| warning["type"] == "duplicate_attribute")
        .unwrap();
    assert_eq!(
        duplicate["message"],
        "Duplicate attribute 'name' at line 2 (first defined on line 2). The last value will be used."
    );
    assert!(duplicate["file"].as_str().unwrap().ends_with("main.crn"));
    let line = source.lines().nth(1).unwrap();
    let start = line[..line.rfind("name").unwrap()].chars().count() + 1;
    assert_eq!(
        duplicate["location"]["range"],
        serde_json::json!({
            "start": {"line": 2, "column": start}, "end": {"line": 2, "column": start + 4}
        })
    );
    let human = validate(fixture.path(), false);
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        format!(
            "Validating...\n✓ 1 resources validated successfully.\n  • mock.test.resource.temp\n⚠ Unused let binding 'temp'. Consider using an anonymous resource instead.\n⚠ {}:Duplicate attribute 'name' at line 2 (first defined on line 2). The last value will be used.\n",
            fixture.path().join("main.crn").display()
        )
    );
    assert!(human.stderr.is_empty());

    std::fs::write(
        fixture.path().join("main.crn"),
        "provider mock {}\nmock.test.resource { name = 'literal-${value}' }\n",
    )
    .unwrap();
    let json = report(&validate(fixture.path(), true), 0, 1);
    assert_eq!(
        json["warnings"][0]["message"],
        carina_core::parser::SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE
    );
    assert_eq!(json["warnings"][0]["severity"], "warning");
}

#[test]
fn wasm_validation_recovery_notices_belong_to_the_report() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let wasm = workspace.join("target/wasm32-wasip2/debug/carina-provider-mock.wasm");
    assert!(wasm.is_file(), "mock WASM fixture must be built");
    std::fs::copy(&wasm, fixture.path().join("local.wasm")).unwrap();
    let installed = fixture
        .path()
        .join(".carina/providers/file/local/local.wasm");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(wasm, installed).unwrap();
    std::fs::write(fixture.path().join("main.crn"), "provider mock { source = 'file://local.wasm' validate_config_trap = true }\nfn checked(value: mock.RecoveryProbe): String { value }\nlet _ = checked('ok')\nattributes { probe: mock.RecoveryProbe = 'ok' }\n").unwrap();
    let run = |json| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(fixture.path())
            .args(["validate", "."])
            .env("NO_COLOR", "1")
            .env("RUST_LOG_STYLE", "never")
            .env("RUST_LOG", "warn")
            .env_remove("CLICOLOR_FORCE");
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    };
    let human = run(false);
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert_eq!(human.status.code(), Some(1), "{stderr}");
    let recovery = "rebuilt poisoned WASM provider validation instance; previous poison reason: WASM provider instance unusable after trap in validate_config";
    assert_eq!(stderr.matches(recovery).count(), 1, "{stderr}");
    assert!(
        stderr.lines().any(|line| line.contains(" WARN ")
            && line.contains("carina_plugin_host::wasm_factory")
            && line.contains(recovery)),
        "{stderr}"
    );
    let json = report(&run(true), 1, 0);
    assert!(!json.to_string().contains("rebuilt poisoned"));
}

#[test]
fn failed_directory_recovery_keeps_ordinary_parser_warnings_in_json_only() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("a.crn"), "backend local {}\n").unwrap();
    let sibling = fixture.path().join("b.crn");
    std::fs::write(&sibling, "backend local {}\nlet note = '${x}'\n").unwrap();
    let json = report(&validate(fixture.path(), true), 1, 1);
    let warning = &json["warnings"][0];
    assert_eq!(
        warning["message"],
        carina_core::parser::SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE
    );
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["location"]["file"], sibling.display().to_string());
    assert_eq!(warning["location"]["range"]["start"]["line"], 2);
    let human = validate(fixture.path(), false);
    assert_eq!(human.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        format!(
            "Error: {}: multiple backend blocks defined\n",
            sibling.display()
        )
    );
    assert!(human.stdout.is_empty());
}

#[test]
fn reference_mismatch_context_is_shared_by_human_json_and_lsp() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "provider mock {}\nlet src = mock.iam.Role { role_name = 'src' }\nmock.test.resource { name = src.max_session_duration }\n";
    std::fs::write(&path, source).unwrap();
    let json = report(&validate(fixture.path(), true), 1, 0);
    let human = validate(fixture.path(), false);
    let stderr = String::from_utf8(human.stderr).unwrap();
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), vec!["mock".into()]);
    let document = Document::new_with_path(
        source.to_string(),
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let lsp = engine.analyze(&document, Some(&path));
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.ends_with("(from src.max_session_duration)"),
        "{json}\n{stderr}\n{lsp:?}"
    );
    assert!(
        stderr.contains(message.strip_prefix("Type mismatch: ").unwrap()),
        "{stderr}"
    );
    assert!(
        lsp.iter().any(|finding| finding.message == message),
        "{lsp:?}"
    );
}

#[test]
fn missing_module_errors_keep_the_same_call_anchor_as_lsp() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "let child = use { source = './missing' }\nchild {}\n";
    std::fs::write(&path, source).unwrap();
    let json = report(&validate(fixture.path(), true), 1, 0);
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(Vec::new()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), Vec::new());
    let document = Document::new_with_path(
        source.to_string(),
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|finding| finding.severity == Some(DiagnosticSeverity::ERROR))
        .unwrap_or_else(|| panic!("{diagnostics:?}"));
    assert_eq!(
        json["errors"][0]["location"]["file"],
        path.display().to_string()
    );
    assert_eq!(
        json["errors"][0]["location"]["range"],
        serde_json::json!({
            "start": {"line": diagnostic.range.start.line + 1, "column": diagnostic.range.start.character + 1},
            "end": {"line": diagnostic.range.end.line + 1, "column": diagnostic.range.end.character + 1}
        })
    );
}

#[test]
fn invalid_upstream_source_expressions_do_not_expose_secrets_on_any_surface() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(Vec::new()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), Vec::new());

    for (expression, expected_expression, forbidden) in [
        ("42", Some("42 "), None),
        (
            "secret('CANARY')",
            Some("secret(<redacted>) "),
            Some("CANARY"),
        ),
        (
            "length(secret('CANARY'))",
            Some("length(secret(<redacted>)) "),
            Some("CANARY"),
        ),
    ] {
        let source = format!("let s = upstream_state {{ source = {expression} }}\n");
        std::fs::write(&path, &source).unwrap();

        let human = validate(fixture.path(), false);
        assert_eq!(human.status.code(), Some(1));
        let human_stdout = String::from_utf8(human.stdout).unwrap();
        let human_stderr = String::from_utf8(human.stderr).unwrap();

        let output = validate(fixture.path(), true);
        let json = report(&output, 1, 0);
        let json_stdout = String::from_utf8(output.stdout).unwrap();
        let document = Document::new_with_path(
            source,
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        if let Some(forbidden) = forbidden {
            assert!(
                !human_stdout.contains(forbidden)
                    && !human_stderr.contains(forbidden)
                    && !json_stdout.contains(forbidden)
                    && diagnostics
                        .iter()
                        .all(|diagnostic| !diagnostic.message.contains(forbidden)),
                "human stdout: {human_stdout:?}\nhuman stderr: {human_stderr:?}\nJSON stdout: {json_stdout:?}\nLSP diagnostics: {diagnostics:?}"
            );
        }
        if let Some(expected_expression) = expected_expression {
            let expected_message = format!(
                "upstream_state 's': 'source' must be a string literal, got: {expected_expression}"
            );
            assert_eq!(json["errors"][0]["message"], expected_message);
            assert!(human_stderr.contains(&expected_message), "{human_stderr}");
            assert!(
                diagnostics[0].message.contains(&expected_message),
                "{diagnostics:?}"
            );
        }
    }
}

#[test]
fn invalid_ignore_changes_expressions_do_not_expose_secrets_on_any_surface() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "provider mock {}\nmock.test.resource {\n  name = 'safe'\n  directives { ignore_changes = [secret('IGNORE_CANARY')] }\n}\n";
    std::fs::write(&path, source).unwrap();
    let human = validate(fixture.path(), false);
    assert_eq!(human.status.code(), Some(1));
    let stdout = String::from_utf8(human.stdout).unwrap();
    let stderr = String::from_utf8(human.stderr).unwrap();
    let output = validate(fixture.path(), true);
    let json = report(&output, 1, 0);
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(Vec::new()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), Vec::new());
    let document = Document::new_with_path(
        source.to_string(),
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    assert!(!diagnostics.is_empty());
    assert!(
        !stdout.contains("IGNORE_CANARY")
            && !stderr.contains("IGNORE_CANARY")
            && !json.to_string().contains("IGNORE_CANARY")
            && diagnostics
                .iter()
                .all(|finding| !finding.message.contains("IGNORE_CANARY")),
        "human stdout: {stdout:?}\nhuman stderr: {stderr:?}\nJSON: {json}\nLSP: {diagnostics:?}"
    );
}

#[test]
fn malformed_secret_source_is_not_in_json() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "let hidden = secret('private-syntax-value') ???\n",
    )
    .unwrap();
    let output = validate(fixture.path(), true);
    let json = report(&output, 1, 0);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-syntax-value"));
    assert!(
        !json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("let hidden")
    );
    assert!(!json["errors"][0]["location"]["range"].is_null());
}

#[test]
fn missing_upstream_sources_are_separate_findings_owned_by_each_declaration_file() {
    let fixture = tempfile::tempdir().unwrap();
    for (file, binding) in [("a.crn", "first"), ("b.crn", "second")] {
        std::fs::write(
            fixture.path().join(file),
            format!(
                "// authored source\nlet {binding} = upstream_state {{ source = './missing' }}\n"
            ),
        )
        .unwrap();
    }
    let json = report(&validate(fixture.path(), true), 2, 0);
    let errors = json["errors"].as_array().unwrap();
    for (file, binding) in [("a.crn", "first"), ("b.crn", "second")] {
        let message = format!("upstream_state '{binding}': source './missing' does not exist");
        let finding = errors
            .iter()
            .find(|finding| finding["message"] == message)
            .unwrap_or_else(|| panic!("missing {message:?}: {json}"));
        assert_eq!(
            finding["location"]["file"],
            fixture.path().join(file).display().to_string()
        );
        assert_eq!(finding["location"]["range"]["start"]["line"], 2);
        assert!(finding["code"].is_null());
    }
    let human = validate(fixture.path(), false);
    assert_eq!(String::from_utf8(human.stdout).unwrap(), "Validating...\n");
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        "Error: upstream_state 'first': source './missing' does not exist\nError: upstream_state 'second': source './missing' does not exist\n"
    );
}

#[test]
fn identical_missing_upstream_sources_keep_distinct_authored_locations() {
    let fixture = tempfile::tempdir().unwrap();
    for file in ["a.crn", "b.crn"] {
        std::fs::write(
            fixture.path().join(file),
            "let shared = upstream_state { source = './missing' }\n",
        )
        .unwrap();
    }
    let json = report(&validate(fixture.path(), true), 3, 0);
    let findings = json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| {
            finding["message"] == "upstream_state 'shared': source './missing' does not exist"
        })
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 2, "{json}");
    for file in ["a.crn", "b.crn"] {
        assert!(
            findings.iter().any(|finding| finding["location"]["file"]
                == fixture.path().join(file).display().to_string()),
            "{json}"
        );
    }
}

#[test]
fn independent_resource_admission_failures_are_separate_diagnostics() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n").unwrap();
    for file in ["a.crn", "b.crn"] {
        std::fs::write(
            fixture.path().join(file),
            "mock.test.resource { name = true }\n",
        )
        .unwrap();
    }
    let json = report(&validate(fixture.path(), true), 2, 0);
    for file in ["a.crn", "b.crn"] {
        assert!(
            json["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["location"]["file"]
                    == fixture.path().join(file).display().to_string()),
            "{json}"
        );
    }
}

#[test]
fn loader_failures_keep_one_diagnostic_per_file() {
    let fixture = tempfile::tempdir().unwrap();
    for file in ["a.crn", "b.crn", "c.crn"] {
        std::fs::write(
            fixture.path().join(file),
            "backend local { path = 'state.json' }\n",
        )
        .unwrap();
    }
    let json = report(&validate(fixture.path(), true), 2, 0);
    for file in ["b.crn", "c.crn"] {
        let finding = json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| {
                finding["location"]["file"] == fixture.path().join(file).display().to_string()
            })
            .unwrap_or_else(|| panic!("{json}"));
        assert_eq!(finding["message"], "multiple backend blocks defined");
        assert!(finding["location"]["range"].is_null());
    }
}

#[test]
fn export_boundary_failures_keep_separate_authored_locations() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "exports {\n  first = secret('first-private')\n  second = secret('second-private')\n}\n",
    )
    .unwrap();
    let json = report(&validate(fixture.path(), true), 2, 0);
    for (name, line) in [("first", 2), ("second", 3)] {
        let message = format!("export '{name}' cannot hold a secret value");
        let finding = json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["message"] == message)
            .unwrap_or_else(|| panic!("{json}"));
        assert_eq!(
            finding["location"]["file"],
            fixture.path().join("main.crn").display().to_string()
        );
        assert_eq!(finding["location"]["range"]["start"]["line"], line);
    }
    assert!(!json.to_string().contains("first-private"));
    assert!(!json.to_string().contains("second-private"));
}

#[test]
fn prefix_failures_keep_separate_resource_locations() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), "provider mock {}\nlet first = mock.test.resource { name_prefix = '' }\nlet second = mock.test.resource { name_prefix = '' }\n").unwrap();
    let json = report(&validate(fixture.path(), true), 2, 2);
    for (name, line) in [("first", 2), ("second", 3)] {
        let finding = json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["message"].as_str().unwrap().contains(name))
            .unwrap_or_else(|| panic!("{json}"));
        assert_eq!(
            finding["location"]["file"],
            fixture.path().join("main.crn").display().to_string()
        );
        assert_eq!(finding["location"]["range"]["start"]["line"], line);
        let warning = json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["message"] == format!("Unused let binding '{name}'"))
            .unwrap_or_else(|| panic!("{json}"));
        assert_eq!(warning["type"], "unused_binding");
        assert_eq!(warning["severity"], "warning");
        assert_eq!(warning["location"]["range"]["start"]["line"], line);
    }
}

#[test]
fn upstream_export_boundary_failures_preserve_individual_findings_and_human_text() {
    let fixture = tempfile::tempdir().unwrap();
    let upstream = fixture.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(upstream.join("main.crn"), "provider mock {}\nlet thing = mock.test.resource { name = 'thing' }\nexports {\n  first = thing\n  second = thing\n}\n").unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        "let source = upstream_state { source = './upstream' }\n",
    )
    .unwrap();
    let json = report(&validate(fixture.path(), true), 2, 0);
    let reasons = [("first", 4), ("second", 5)].map(|(name, line)| (format!("export '{name}': inferred resource handle mock.test.resource cannot cross a state boundary; export an identity value such as mock.test.resource.Id instead"), line));
    for (message, line) in &reasons {
        let finding = json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["message"] == *message)
            .unwrap_or_else(|| panic!("{json}"));
        assert_eq!(
            finding["location"]["file"],
            upstream.join("main.crn").display().to_string()
        );
        assert_eq!(finding["location"]["range"]["start"]["line"], *line);
    }
    let human = validate(fixture.path(), false);
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        format!(
            "Error: {}:1:40: upstream_state `source`: failed to parse source `./upstream`: {}\nError: {}\n",
            fixture.path().join("main.crn").display(),
            reasons[0].0,
            reasons[1].0
        )
    );
}

#[test]
fn nested_parse_failures_never_serialize_source_snippets() {
    for upstream in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/main.crn"),
            "let hidden = secret('private-nested-value') ???\n",
        )
        .unwrap();
        let source = if upstream {
            "let child = upstream_state { source = './child' }\n"
        } else {
            "let child = use { source = './child' }\nchild {}\n"
        };
        std::fs::write(fixture.path().join("main.crn"), source).unwrap();
        let output = validate(fixture.path(), true);
        let json = report(&output, 1, 0);
        assert!(!json.to_string().contains("private-nested-value"), "{json}");
        assert!(
            !json["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("let hidden"),
            "{json}"
        );
    }
}
