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
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine().analyze(&document, Some(&path))
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
