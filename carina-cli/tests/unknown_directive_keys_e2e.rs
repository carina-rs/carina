//! Directory-scoped CLI/LSP diagnostic parity for carina#4412.
use std::{path::Path, process::Command, sync::Arc};

use carina_core::{parser::ProviderContext, provider::ProviderFactory, schema::SchemaRegistry};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

fn fixture() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unknown_directive_keys");
    for name in ["main.crn", "providers.crn", "resources.crn"] {
        std::fs::copy(source.join(name), directory.path().join(name)).unwrap();
    }
    directory
}

fn validate(directory: &Path, json: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .args(["validate", "."])
        .current_dir(directory)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    if json {
        command.arg("--json");
    }
    command.output().unwrap()
}

fn engine() -> DiagnosticEngine {
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
    DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()])
}

fn lsp(directory: &Path, name: &str) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let path = directory.join(name);
    lsp_buffer(directory, name, std::fs::read_to_string(&path).unwrap())
}

fn lsp_buffer(
    directory: &Path,
    name: &str,
    source: String,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let path = directory.join(name);
    let document = Document::new_with_path(
        source,
        path.clone(),
        Arc::new(ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine().analyze(&document, Some(&path))
}

#[test]
fn non_assignment_directive_entries_have_identical_cli_and_lsp_findings() {
    for (entry, width) in [
        ("prevent_destroy {}", 15),
        ("unsupported {}", 11),
        ("let x = 1", 9),
    ] {
        let directory = fixture();
        std::fs::write(directory.path().join("resources.crn"), format!("mock.test.resource {{\n  name = 'example'\n  directives {{\n    {entry}\n  }}\n}}\n")).unwrap();
        let message = "Directive entries must be 'key = value' assignments";
        let human = validate(directory.path(), false);
        let json = validate(directory.path(), true);
        assert_eq!(human.status.code(), Some(1), "{entry}: {human:?}");
        assert_eq!(json.status.code(), Some(1), "{entry}: {json:?}");
        let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{entry}: {report:#}");
        assert_eq!(errors[0]["message"], message);
        assert_eq!(errors[0]["location"]["file"], "resources.crn");
        assert_eq!(
            errors[0]["location"]["range"],
            serde_json::json!({"start":{"line":4,"column":5}, "end":{"line":4,"column":5+width}})
        );
        assert!(
            String::from_utf8(human.stderr)
                .unwrap()
                .contains(&format!("resources.crn:4:5: {message}"))
        );
        let diagnostics = lsp(directory.path(), "resources.crn");
        assert_eq!(diagnostics.len(), 1, "{entry}: {diagnostics:#?}");
        assert_eq!(diagnostics[0].message, message);
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(3, 4), Position::new(3, 4 + width))
        );
        for name in ["main.crn", "providers.crn"] {
            assert!(lsp(directory.path(), name).is_empty(), "{entry}: {name}");
        }
    }
}

#[test]
fn duplicate_directives_collect_unknown_keys_in_both_blocks_in_cli_and_lsp() {
    let directory = fixture();
    std::fs::write(directory.path().join("resources.crn"), "mock.test.resource {\n  name = 'example'\n  directives { prevent_destory = true }\n  directives { provder = absent }\n}\n").unwrap();
    let human = validate(directory.path(), false);
    let json = validate(directory.path(), true);
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    assert_eq!(json.status.code(), Some(1), "{json:?}");
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    let diagnostics = lsp(directory.path(), "resources.crn");
    assert_eq!(errors.len(), 3, "{report:#}");
    assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
    let stderr = String::from_utf8(human.stderr).unwrap();
    for (message, line, column, width) in [
        (
            "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?",
            3,
            16,
            15,
        ),
        (
            "Duplicate attribute 'directives' (first defined on line 3)",
            4,
            3,
            10,
        ),
        (
            "Unknown directive 'provder', did you mean 'provider'?",
            4,
            16,
            7,
        ),
    ] {
        let error = errors
            .iter()
            .find(|error| error["message"] == message)
            .unwrap();
        assert_eq!(
            error["location"]["range"],
            serde_json::json!({"start":{"line":line,"column":column}, "end":{"line":line,"column":column+width}})
        );
        assert!(stderr.contains(&format!("resources.crn:{line}:{column}: {message}")));
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap();
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostic.range,
            Range::new(
                Position::new(line - 1, column - 1),
                Position::new(line - 1, column + width - 1)
            )
        );
    }
}

#[test]
fn duplicate_let_and_directives_are_both_reported_in_the_unsaved_lsp_buffer() {
    let directory = fixture();
    let source = "let repeated = 1\nlet repeated = 2\nmock.test.resource {\n  name = 'example'\n  directives {}\n  directives {}\n}\n";
    let diagnostics = lsp_buffer(directory.path(), "resources.crn", source.into());
    for (prefix, line, column, width) in [
        ("duplicate binding 'repeated'", 1, 4, 8),
        ("Duplicate attribute 'directives'", 5, 2, 10),
    ] {
        let matching: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.starts_with(prefix))
            .collect();
        assert_eq!(matching.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            matching[0].range,
            Range::new(
                Position::new(line, column),
                Position::new(line, column + width)
            )
        );
    }
}

#[test]
fn duplicate_let_does_not_hide_collected_directive_findings_in_cli() {
    let directory = fixture();
    std::fs::write(directory.path().join("resources.crn"), "let repeated = 1\nlet repeated = 2\nmock.test.resource {\n  name = 'example'\n  directives {}\n  directives { provder = absent }\n}\n").unwrap();
    let output = validate(directory.path(), true);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for prefix in [
        "Duplicate attribute 'directives'",
        "Unknown directive 'provder'",
    ] {
        let matching: Vec<_> = report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|error| error["message"].as_str().unwrap().starts_with(prefix))
            .collect();
        assert_eq!(matching.len(), 1, "{report:#}");
        assert_eq!(matching[0]["location"]["file"], "resources.crn");
    }
}

#[test]
fn duplicate_directives_in_a_sibling_keep_the_require_scope_complete() {
    let directory = fixture();
    std::fs::write(
        directory.path().join("resources.crn"),
        "mock.test.resource {\n  name = 'example'\n  directives {}\n  directives {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "require false, 'root guard'\n",
    )
    .unwrap();
    let diagnostics = lsp(directory.path(), "main.crn");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "require failed: root guard"),
        "{diagnostics:#?}"
    );
    let output = validate(directory.path(), true);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["message"] == "require failed: root guard"),
        "{report:#}"
    );
}

#[test]
fn unknown_directive_key_has_identical_cli_and_lsp_message_and_key_range() {
    let directory = fixture();
    let message = "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?";
    let human = validate(directory.path(), false);
    let json = validate(directory.path(), true);
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    assert_eq!(json.status.code(), Some(1), "{json:?}");
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        format!("Error: ./resources.crn:4:5: {message}\n")
    );
    assert!(json.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["status"], "error");
    assert_eq!(
        report["errors"],
        serde_json::json!([{
            "severity": "error",
            "code": null,
            "message": message,
            "location": {
                "file": "resources.crn",
                "range": {
                    "start": {"line": 4, "column": 5},
                    "end": {"line": 4, "column": 20}
                }
            }
        }])
    );
    let diagnostics = lsp(directory.path(), "resources.crn");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].message, message);
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostics[0].source.as_deref(), Some("carina"));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(3, 4), Position::new(3, 19))
    );
    for name in ["main.crn", "providers.crn"] {
        assert!(lsp(directory.path(), name).is_empty(), "{name}");
    }
}

#[test]
fn lsp_reports_unknown_directive_key_in_sibling_file() {
    let directory = fixture();
    let diagnostics = lsp(directory.path(), "resources.crn");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(3, 4), Position::new(3, 19))
    );
}

#[test]
fn unknown_directive_keys_do_not_hide_other_directory_errors() {
    let directory = fixture();
    std::fs::write(
        directory.path().join("main.crn"),
        "mock.test.resource { name = 123 }\n",
    )
    .unwrap();
    let output = validate(directory.path(), true);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{report:#}");
    assert!(
        errors.iter().any(|error| error["message"]
            .as_str()
            .unwrap()
            .contains("Unknown directive 'prevent_destory'")),
        "{report:#}"
    );
    assert!(
        errors.iter().any(|error| {
            error["message"] == "Type mismatch: expected String, got Int"
                && error["location"]["file"] == "main.crn"
        }),
        "{report:#}"
    );
    assert!(
        lsp(directory.path(), "resources.crn")
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("Unknown directive 'prevent_destory'"))
    );
}

#[test]
fn multiple_unknown_directive_keys_include_a_key_without_a_close_match() {
    let directory = fixture();
    std::fs::write(
        directory.path().join("resources.crn"),
        "mock.test.resource {\n  name = \"example\"\n  directives {\n    prevent_destory = true\n    unsupported_key = true\n  }\n}\n",
    )
    .unwrap();
    let human = validate(directory.path(), false);
    let json = validate(directory.path(), true);
    assert_eq!(human.status.code(), Some(1));
    assert_eq!(json.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    let diagnostics = lsp(directory.path(), "resources.crn");
    assert_eq!(errors.len(), 2, "{report:#}");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    let stderr = String::from_utf8(human.stderr).unwrap();
    for (index, message) in [
        "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?",
        "Unknown directive 'unsupported_key'",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(errors[index]["message"], message);
        assert_eq!(diagnostics[index].message, message);
        assert_eq!(diagnostics[index].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[index].range,
            Range::new(
                Position::new(3 + index as u32, 4),
                Position::new(3 + index as u32, 19),
            )
        );
        assert_eq!(
            errors[index]["location"]["range"]["start"]["line"],
            4 + index
        );
        assert_eq!(errors[index]["location"]["range"]["start"]["column"], 5);
        assert_eq!(errors[index]["location"]["range"]["end"]["column"], 20);
        assert!(stderr.contains(&format!("resources.crn:{}:5: {message}", 4 + index)));
    }
}

#[test]
fn unknown_directive_key_survives_an_unparseable_sibling() {
    let directory = fixture();
    std::fs::write(directory.path().join("main.crn"), "let broken = ???\n").unwrap();
    let output = validate(directory.path(), true);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{report:#}");
    assert!(errors.iter().any(|error| error["message"]
        == "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?"));
    assert!(
        errors
            .iter()
            .any(|error| error["location"]["file"] == "main.crn")
    );
    assert!(
        lsp(directory.path(), "resources.crn")
            .iter()
            .any(|diagnostic| {
                diagnostic.message
                    == "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?"
            })
    );
}

#[test]
fn unknown_directive_key_in_an_uncalled_module_is_collected_once() {
    let directory = fixture();
    let module = directory.path().join("child");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "attributes { ready: Bool = true }\n",
    )
    .unwrap();
    std::fs::rename(
        directory.path().join("resources.crn"),
        module.join("resources.crn"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let child = use { source = \"./child\" }\n",
    )
    .unwrap();
    let output = validate(directory.path(), true);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report:#}");
    assert_eq!(
        errors[0]["message"],
        "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?"
    );
    assert!(
        errors[0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("child/resources.crn")
    );
}

#[test]
fn nested_directives_get_the_ordinary_schema_error_in_cli_and_lsp() {
    let directory = fixture();
    std::fs::write(directory.path().join("resources.crn"),
        "mock.test.resource {\n  name = 'example'\n  rule {\n    directives { prevent_destory = true }\n  }\n}\n").unwrap();
    let human = validate(directory.path(), false);
    let json = validate(directory.path(), true);
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    assert_eq!(json.status.code(), Some(1), "{json:?}");
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report:#}");
    let message = "List item at index 0: Unknown field 'directives' in Rule";
    assert_eq!(errors[0]["message"], message);
    assert_eq!(errors[0]["location"]["file"], "resources.crn");
    assert_eq!(
        errors[0]["location"]["range"],
        serde_json::json!({
            "start": {"line": 3, "column": 3}, "end": {"line": 3, "column": 7}
        })
    );
    assert!(String::from_utf8(human.stderr).unwrap().contains(message));
    let diagnostics = lsp(directory.path(), "resources.crn");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Unknown field 'directives' in Rule for 'rule[0].directives'"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(3, 4), Position::new(3, 14))
    );

    let source = std::fs::read_to_string(directory.path().join("resources.crn")).unwrap();
    std::fs::write(
        directory.path().join("resources.crn"),
        source.replace("directives", "ordinary"),
    )
    .unwrap();
    let ordinary = validate(directory.path(), true);
    let ordinary: serde_json::Value = serde_json::from_slice(&ordinary.stdout).unwrap();
    assert_eq!(
        ordinary["errors"][0]["message"],
        message.replace("'directives'", "'ordinary'")
    );
    assert_eq!(ordinary["errors"][0]["location"], errors[0]["location"]);
    let ordinary = lsp(directory.path(), "resources.crn");
    assert_eq!(ordinary.len(), 1, "{ordinary:#?}");
    assert_eq!(ordinary[0].severity, diagnostics[0].severity);
    assert_eq!(
        ordinary[0].message,
        diagnostics[0].message.replace("directives", "ordinary")
    );
    assert_eq!(ordinary[0].range.start, diagnostics[0].range.start);
    assert_eq!(ordinary[0].range.end, Position::new(3, 12));
}

#[test]
fn quoted_directive_keys_are_accepted_by_cli_and_lsp() {
    let directory = fixture();
    std::fs::write(directory.path().join("resources.crn"),
        "mock.test.resource {\n  name = 'example'\n  directives {\n    \"prevent_destroy\" = true\n    'ignore_changes' = [name]\n    \"depends_on\" = []\n  }\n}\n").unwrap();
    for json in [false, true] {
        let output = validate(directory.path(), json);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }
    for name in ["main.crn", "providers.crn", "resources.crn"] {
        let diagnostics = lsp(directory.path(), name);
        assert!(diagnostics.is_empty(), "{name}: {diagnostics:#?}");
    }
}

#[test]
fn duplicate_directives_have_identical_cli_and_lsp_errors_at_the_second_name() {
    let directory = fixture();
    std::fs::write(directory.path().join("resources.crn"),
        "mock.test.resource {\n  name = 'example'\n  directives { prevent_destroy = true }\n  directives { force_delete = true }\n}\n").unwrap();
    let message = "Duplicate attribute 'directives' (first defined on line 3)";
    let human = validate(directory.path(), false);
    let json = validate(directory.path(), true);
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    assert_eq!(json.status.code(), Some(1), "{json:?}");
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report:#}");
    assert_eq!(errors[0]["message"], message);
    assert_eq!(errors[0]["severity"], "error");
    assert_eq!(errors[0]["location"]["file"], "resources.crn");
    assert_eq!(
        errors[0]["location"]["range"],
        serde_json::json!({
            "start": {"line": 4, "column": 3}, "end": {"line": 4, "column": 13}
        })
    );
    assert!(
        String::from_utf8(human.stderr)
            .unwrap()
            .contains(&format!("resources.crn:4:3: {message}"))
    );
    let diagnostics = lsp(directory.path(), "resources.crn");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].message, message);
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(3, 2), Position::new(3, 12))
    );
    for name in ["main.crn", "providers.crn"] {
        assert!(lsp(directory.path(), name).is_empty(), "{name}");
    }
}
