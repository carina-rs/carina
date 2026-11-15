//! Independent directive and conversion findings share the existing run exit.
use std::{process::Command, sync::Arc};

use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use carina_lsp::{
    backend::{FactoryBuilder, ProviderStates, analyze_document, reload_provider_states},
    document::Document,
    factories::{build_factories, install_prober},
};

#[tokio::test(flavor = "multi_thread")]
async fn conversion_and_directive_findings_share_one_exit_without_hiding_each_other() {
    for converted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        let suffix = if converted { " as String" } else { "" };
        let source = format!(
            "let value = 'ok'{suffix}\nmock.test.resource {{\n  name = value\n  directives {{ prevent_destory = true }}\n}}\n"
        );
        let path = directory.path().join("main.crn");
        std::fs::write(&path, &source).unwrap();

        let validate = |json| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .current_dir(directory.path())
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .args(["validate", "."]);
            if json {
                command.arg("--json");
            }
            command.output().unwrap()
        };
        let output = validate(true);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = json["errors"].as_array().unwrap();
        assert_eq!(errors.len(), if converted { 2 } else { 1 }, "{json:#}");
        let directive = "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?";
        assert_eq!(
            errors
                .iter()
                .filter(|error| error["message"] == directive)
                .count(),
            1,
            "{json:#}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|error| error["code"] == "conversion_target")
                .count(),
            usize::from(converted),
            "{json:#}"
        );
        let human = validate(false);
        assert_eq!(human.status.code(), Some(1));
        let message = String::from_utf8(human.stderr).unwrap();
        assert_eq!(message.matches(directive).count(), 1, "{message}");
        assert_eq!(
            message
                .matches("conversion target must be a loaded named scalar type")
                .count(),
            usize::from(converted),
            "{message}"
        );

        let home = tempfile::tempdir().unwrap();
        let dirs = UserDirs::under(home.path());
        let builder: FactoryBuilder =
            Arc::new(move |configs| build_factories(Ok(&dirs), configs, &Default::default()));
        let states = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
        reload_provider_states(directory.path(), &builder, Some(&install_prober()), &states).await;
        let doc = Document::new_with_path(
            source,
            path.clone(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        );
        let diagnostics = analyze_document(&states.into_inner(), &doc, Some(&path));
        assert_eq!(diagnostics.len(), errors.len(), "{diagnostics:#?}");
        for error in errors {
            assert_eq!(
                diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        diagnostic.message == error["message"].as_str().unwrap()
                    })
                    .count(),
                1,
                "{diagnostics:#?}"
            );
        }
    }
}
