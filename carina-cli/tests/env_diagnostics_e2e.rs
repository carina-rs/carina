//! Environment strings are public unless they pass through secret() (#4409).

use std::process::Command;
use std::sync::Arc;

use carina_core::EnvironmentSnapshot;
use carina_core::provider::{ProviderFactory, loaded_provider_snapshot_from_factories};
use carina_core::schema::SchemaRegistry;
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};

#[test]
fn env_validation_directory_cli_text_json_and_lsp_preserves_secret_boundary() {
    const VARIABLE: &str = "CARINA_4409_DIAGNOSTIC_VALUE";
    const VALUE: &str = "env-diagnostic-value";
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let path = directory.path().join("main.crn");
    let environment = EnvironmentSnapshot::new([(VARIABLE.to_string(), VALUE.into())]);
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let providers = loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        environment.clone(),
    );
    let context = Arc::new(providers.provider_context().as_provider_context().clone());
    let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);
    let mut observations = Vec::new();

    // Exercise the secret control before the ordinary env case, so a red run
    // also proves that all three surfaces already protect secret(env(...)).
    for (expression, hidden) in [
        (format!("secret(env('{VARIABLE}'))"), true),
        (format!("env('{VARIABLE}')"), false),
    ] {
        let source = format!("mock.test.pattern_modes {{\n  full = {expression}\n}}\n");
        std::fs::write(&path, &source).unwrap();
        let run = |json| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .current_dir(directory.path())
                .args(["validate", "."])
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .env(VARIABLE, VALUE);
            if json {
                command.arg("--json");
            }
            command.output().unwrap()
        };
        let human = run(false);
        let json = run(true);
        assert_eq!(human.status.code(), Some(1));
        assert_eq!(json.status.code(), Some(1));
        assert!(json.stderr.is_empty(), "{json:?}");
        let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        let message = errors[0]["message"].as_str().unwrap();
        let human = format!(
            "{}{}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr)
        );
        assert!(human.contains(message), "{human}\n{report}");

        let provider_path = directory.path().join("providers.crn");
        let provider_document = Document::new_with_path(
            std::fs::read_to_string(&provider_path).unwrap(),
            provider_path.clone(),
            context.clone(),
        );
        assert!(
            engine
                .analyze(&provider_document, Some(&provider_path))
                .is_empty()
        );
        let document = Document::new_with_path(source, path.clone(), context.clone());
        let diagnostics = engine.analyze(&document, Some(&path));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            message, diagnostics[0].message,
            "CLI={report}\nLSP={diagnostics:#?}"
        );
        assert!(
            diagnostics[0]
                .message
                .contains("does not match required pattern")
        );

        for (surface, rendered) in [
            ("CLI text", human),
            ("CLI JSON", String::from_utf8(json.stdout).unwrap()),
            ("LSP", format!("{diagnostics:#?}")),
        ] {
            observations.push((
                expression.clone(),
                surface,
                rendered.contains(VALUE) != hidden && (!hidden || rendered.contains("(secret)")),
                rendered,
            ));
        }
    }
    assert!(
        observations.iter().all(|(_, _, correct, _)| *correct),
        "env() must be shown and secret(env(...)) must stay masked: {observations:#?}"
    );
}
