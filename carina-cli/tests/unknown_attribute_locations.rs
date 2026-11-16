//! Directory-scoped unknown-key locations shared by validate JSON and the LSP.
use std::{path::Path, process::Command, sync::Arc};

use carina_core::{parser::ProviderContext, provider::ProviderFactory, schema::SchemaRegistry};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use tower_lsp::lsp_types::{Position, Range};

fn editor(directory: &Path, member: &str) -> Vec<tower_lsp::lsp_types::Diagnostic> {
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
    let path = directory.join(member);
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine.analyze(&document, Some(&path))
}

#[test]
fn unknown_attribute_sibling_matches_text_json_and_lsp() {
    let directory = tempfile::tempdir().unwrap();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unknown_attribute_locations");
    for name in ["providers.crn", "main.crn", "sibling.crn"] {
        std::fs::copy(fixture.join(name), directory.path().join(name)).unwrap();
    }
    let validate = |json| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .args(["validate", "."])
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE");
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    };
    let json = validate(true);
    assert_eq!(json.status.code(), Some(1), "{json:?}");
    assert!(json.stderr.is_empty(), "{json:?}");
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    eprintln!("sibling CLI JSON: {report}");
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 4, "{report:#}");
    let human = validate(false);
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    let human = String::from_utf8(human.stderr).unwrap();
    let messages = errors
        .iter()
        .map(|error| error["message"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        &messages[..2],
        ["Unknown attribute 'typo'", "Unknown attribute 'multiline'"]
    );
    assert!(messages[2].contains("struct_typo"), "{messages:?}");
    assert!(messages[3].contains("block_typo"), "{messages:?}");
    assert_eq!(
        human.lines().collect::<Vec<_>>(),
        messages
            .iter()
            .map(|message| format!("Error: mock.test.resource.<pending>: {message}"))
            .collect::<Vec<_>>()
    );
    let diagnostics = editor(directory.path(), "sibling.crn")
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("Unknown attribute")
                || diagnostic.message.contains("Unknown field")
        })
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 4, "{diagnostics:#?}");
    eprintln!("sibling LSP: {diagnostics:#?}");
    assert!(
        editor(directory.path(), "main.crn")
            .iter()
            .all(
                |diagnostic| !diagnostic.message.contains("Unknown attribute")
                    && !diagnostic.message.contains("Unknown field")
            )
    );
    let source = std::fs::read_to_string(directory.path().join("sibling.crn")).unwrap();
    let position = |range: &serde_json::Value, endpoint: &str| {
        let line = range[endpoint]["line"].as_u64().unwrap() as usize - 1;
        let column = range[endpoint]["column"].as_u64().unwrap() as usize - 1;
        let utf16 = source
            .lines()
            .nth(line)
            .unwrap()
            .chars()
            .take(column)
            .map(char::len_utf16)
            .sum::<usize>();
        Position::new(line as u32, utf16 as u32)
    };
    for error in errors {
        let key = ["'typo'", "'multiline'", "'struct_typo'", "'block_typo'"]
            .into_iter()
            .find(|key| error["message"].as_str().unwrap().contains(key))
            .unwrap();
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message.contains(key))
            .unwrap();
        assert_eq!(error["location"]["file"], "sibling.crn");
        let range = &error["location"]["range"];
        assert_eq!(
            Range::new(position(range, "start"), position(range, "end")),
            diagnostic.range,
            "{}",
            diagnostic.message
        );
    }
}
