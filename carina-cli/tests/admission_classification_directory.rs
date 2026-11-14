use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use carina_lsp::admission_coverage::{
    AdmissionCoverage, ExpandedDeclineReason, with_admission_coverage,
};
use carina_lsp::backend::{
    FactoryBuilder, ProviderStates, analyze_document, reload_provider_states,
};
use carina_lsp::document::Document;
use carina_lsp::factories::{build_factories, install_prober};

struct Editor {
    states: ProviderStates,
    _home: tempfile::TempDir,
}

impl Editor {
    async fn load(root: &Path) -> Self {
        let home = tempfile::tempdir().unwrap();
        let dirs = UserDirs::under(home.path());
        let builder: FactoryBuilder =
            Arc::new(move |configs| build_factories(Ok(&dirs), configs, &Default::default()));
        let states = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
        let reload = reload_provider_states(root, &builder, Some(&install_prober()), &states).await;
        assert!(reload.warnings.is_empty(), "{:?}", reload.warnings);
        assert!(reload.schemas > 0);
        Self {
            states: states.into_inner(),
            _home: home,
        }
    }

    fn analyze(&self, path: &Path) -> (Vec<tower_lsp::lsp_types::Diagnostic>, AdmissionCoverage) {
        self.analyze_text(path, std::fs::read_to_string(path).unwrap())
    }

    fn analyze_text(
        &self,
        path: &Path,
        text: String,
    ) -> (Vec<tower_lsp::lsp_types::Diagnostic>, AdmissionCoverage) {
        let document = Document::new_with_path(
            text,
            path.to_path_buf(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        );
        with_admission_coverage(|| analyze_document(&self.states, &document, Some(path)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multifile_validation_and_editor_admission_diagnostics() {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/admission_classification");
    let mut records = Vec::new();
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&root)
            .args(["validate", "."])
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE");
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        records.push(serde_json::json!({
            "mode": if json { "json" } else { "text" },
            "status": output.status.code(),
            "stdout": String::from_utf8(output.stdout).unwrap().replace(root.to_str().unwrap(), "$PROJECT"),
            "stderr": String::from_utf8(output.stderr).unwrap().replace(root.to_str().unwrap(), "$PROJECT"),
        }));
    }
    let editor = Editor::load(&root).await;
    for name in [
        "main.crn",
        "exports.crn",
        "providers.crn",
        "backend.crn",
        "module/main.crn",
        "module/attributes.crn",
    ] {
        let path = root.join(name);
        let (diagnostics, coverage) = editor.analyze(&path);
        let diagnostics = serde_json::to_string(&diagnostics)
            .unwrap()
            .replace(root.to_str().unwrap(), "$PROJECT");
        records.push(serde_json::json!({"document": name, "coverage": coverage, "diagnostics": serde_json::from_str::<serde_json::Value>(&diagnostics).unwrap()}));
    }
    insta::assert_snapshot!(serde_json::to_string_pretty(&records).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_analysis_exercises_all_three_resource_admission_branches() {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/admission_classification");
    let editor = Editor::load(&root).await;
    let path = root.join("main.crn");
    let source = std::fs::read_to_string(&path).unwrap();
    let line = |prefix: &str| {
        source
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap()
    };
    // Keep the real module call, and isolate one root resource in the opened
    // buffer. Its module-origin resource contributes one expanded admission
    // in every case; the remaining counters identify this resource's route.
    let setup = format!("{}\n{}", line("let choice ="), line("let child ="));
    for (case, prefix, relative, expanded, decline) in [
        ("relative entry", "let direct =", 1, 1, None),
        ("composition preflight", "let composed =", 0, 2, None),
        (
            "declined expansion",
            "mock.test.resource {",
            1,
            1,
            Some(ExpandedDeclineReason::MissingResource),
        ),
    ] {
        let buffer = format!("{setup}\n{}\n", line(prefix));
        let (diagnostics, coverage) = editor.analyze_text(&path, buffer);
        assert_eq!(
            coverage.relative.reached_admission, relative,
            "{case}: {coverage:?}; {diagnostics:?}"
        );
        assert_eq!(
            coverage.expanded.reached_admission, expanded,
            "{case}: {coverage:?}; {diagnostics:?}"
        );
        let expected = decline
            .into_iter()
            .map(|reason| (reason, 1))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            coverage.expanded_declined_fell_back, expected,
            "{case}: {coverage:?}; {diagnostics:?}"
        );
    }
}
