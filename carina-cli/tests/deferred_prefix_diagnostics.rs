//! carina#4461: one core finding across directory validate, plan, and the editor.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use carina_lsp::backend::{
    FactoryBuilder, ProviderStates, analyze_document, reload_provider_states,
};
use carina_lsp::document::Document;
use carina_lsp::factories::{build_factories, install_prober};
use serde_json::json;
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

const SOURCE: &str = "let src = mock.test.resource { name = 'source' }\n";

struct Fixture {
    directory: tempfile::TempDir,
    files: Vec<String>,
    subject: String,
}

impl Fixture {
    fn new(main: &str, lets: &str, child: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for (name, text) in [
            ("providers.crn", "provider mock {}\n"),
            (
                "backend.crn",
                "backend local { path = 'carina.state.json' }\n",
            ),
            ("lets.crn", lets),
            ("main.crn", main),
        ]
        .into_iter()
        .chain(
            child
                .then_some([
                    ("child/arguments.crn", "arguments {\n  prefix: String\n}\n"),
                    (
                        "child/main.crn",
                        "mock.test.resource {\n  name_prefix = prefix\n}\n",
                    ),
                ])
                .into_iter()
                .flatten(),
        ) {
            let path = directory.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            files.push(name.to_owned());
        }
        Self {
            directory,
            files,
            subject: "mock.test.resource.<pending>".into(),
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn cli(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(arguments)
            .current_dir(self.root())
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("RUST_LOG")
            .output()
            .unwrap()
    }

    async fn assert_editor(
        &self,
        message: Option<&str>,
        authored: &str,
        range: Range,
        module: bool,
    ) {
        let home = tempfile::tempdir().unwrap();
        let dirs = UserDirs::under(home.path());
        let builder: FactoryBuilder =
            Arc::new(move |configs| build_factories(Ok(&dirs), configs, &Default::default()));
        let states = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
        let reload =
            reload_provider_states(self.root(), &builder, Some(&install_prober()), &states).await;
        assert!(reload.warnings.is_empty(), "{:?}", reload.warnings);
        assert!(reload.schemas > 0);
        let states = states.into_inner();
        for file in &self.files {
            let path = self.root().join(file);
            let document = Document::new_with_path(
                std::fs::read_to_string(&path).unwrap(),
                path.clone(),
                Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
            );
            let diagnostics = analyze_document(&states, &document, Some(&path));
            let expected_range = if file == authored {
                Some(range)
            } else if module && file == "main.crn" {
                Some(Range::new(Position::new(2, 11), Position::new(4, 1)))
            } else {
                None
            };
            if let (Some(message), Some(expected_range)) = (message, expected_range) {
                assert_eq!(diagnostics.len(), 1, "{file}: {diagnostics:#?}");
                assert_eq!(diagnostics[0].message, message, "{file}");
                assert_eq!(diagnostics[0].range, expected_range, "{file}");
                assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
            } else {
                assert!(diagnostics.is_empty(), "{file}: {diagnostics:#?}");
            }
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

async fn assert_rejected(
    fixture: Fixture,
    dependency: &str,
    authored: &str,
    range: Range,
    module: bool,
) {
    let message = format!(
        "'name_prefix' must be known at plan time to generate 'name', but it depends on {dependency}, which is known only after apply"
    );
    let human = format!("Error: {}: {message}\n", fixture.subject);
    let output = fixture.cli(&["validate", "."]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output), format!("Validating...\n{human}"));

    let output = fixture.cli(&["validate", ".", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty(), "{output:?}");
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        actual,
        json!({
            "status": "error", "resource_count": 0, "resources": [],
            "errors": [{
                "message": message, "severity": "error", "code": null,
                "location": {"file": authored, "range": {
                    "start": {"line": range.start.line + 1, "column": range.start.character + 1},
                    "end": {"line": range.end.line + 1, "column": range.end.character + 1}
                }}
            }]
        })
    );

    let output = fixture.cli(&["plan", "."]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output), human);
    fixture
        .assert_editor(Some(&message), authored, range, module)
        .await;
}

fn assignment(end: u32) -> Range {
    Range::new(Position::new(1, 2), Position::new(1, end))
}

#[tokio::test]
async fn deferred_prefix_a_direct_directory() {
    assert_rejected(
        Fixture::new(
            "mock.test.resource {\n  name_prefix = src.identifier\n}\n",
            SOURCE,
            false,
        ),
        "'src.identifier'",
        "main.crn",
        assignment(30),
        false,
    )
    .await;
}

#[tokio::test]
async fn deferred_prefix_b_known_directory_control() {
    let fixture = Fixture::new(
        "mock.test.resource {\n  name_prefix = src.name\n}\n",
        SOURCE,
        false,
    );
    let output = fixture.cli(&["validate", ".", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["resource_count"], 2);
    assert!(result.get("errors").is_none(), "{result:#}");
    let resources = result["resources"].as_array().unwrap();
    let expected = format!(
        "Validating...\n✓ 2 resources validated successfully.\n  • {}\n  • {}\n",
        resources[0].as_str().unwrap(),
        resources[1].as_str().unwrap()
    );
    let output = fixture.cli(&["validate", "."]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(text(&output), expected);
    fixture
        .assert_editor(None, "main.crn", assignment(24), false)
        .await;
    let output = fixture.cli(&["init", "."]);
    assert!(output.status.success(), "{output:?}");
    let output = fixture.cli(&["plan", "."]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("2 to add"), "{}", text(&output));
}

#[tokio::test]
async fn deferred_prefix_c1_interpolation_directory() {
    assert_rejected(
        Fixture::new(
            "mock.test.resource {\n  name_prefix = \"${src.identifier}-x\"\n}\n",
            SOURCE,
            false,
        ),
        "'src.identifier'",
        "main.crn",
        assignment(37),
        false,
    )
    .await;
}

#[tokio::test]
async fn deferred_prefix_c2_alias_directory() {
    assert_rejected(
        Fixture::new(
            "mock.test.resource {\n  name_prefix = p\n}\n",
            &format!("{SOURCE}let p = src.identifier\n"),
            false,
        ),
        "'src.identifier' (through 'p')",
        "main.crn",
        assignment(17),
        false,
    )
    .await;
}

#[tokio::test]
async fn deferred_prefix_d_module_argument_directory_publication_sites() {
    assert_rejected(Fixture::new("let sink_module = use { source = './child' }\n\nlet sink = sink_module {\n  prefix = src.identifier\n}\n", SOURCE, true), "module argument 'prefix'", "child/main.crn", assignment(22), true).await;
}

#[tokio::test]
async fn deferred_prefix_multiline_value_uses_key_range() {
    assert_rejected(
        Fixture::new(
            "mock.test.resource {\n  name_prefix = \"${src.identifier\n  }-x\"\n}\n",
            SOURCE,
            false,
        ),
        "'src.identifier'",
        "main.crn",
        assignment(13),
        false,
    )
    .await;
}

#[tokio::test]
async fn deferred_prefix_several_references_join_with_and() {
    assert_rejected(Fixture::new("mock.test.resource {\n  name_prefix = \"${a.identifier}-${b.identifier}\"\n}\n", "let a = mock.test.resource { name = 'a' }\nlet b = mock.test.resource { name = 'b' }\n", false), "'a.identifier' and 'b.identifier'", "main.crn", assignment(49), false).await;
}

#[tokio::test]
async fn deferred_prefix_known_alias_does_not_claim_direct_dependency() {
    let main = "mock.test.resource {\n  name_prefix = \"${p}-${src.identifier}\"\n}\n";
    let range = assignment(main.lines().nth(1).unwrap().len() as u32);
    assert_rejected(
        Fixture::new(main, &format!("{SOURCE}let p = 'known'\n"), false),
        "'src.identifier'",
        "main.crn",
        range,
        false,
    )
    .await;
}

#[tokio::test]
async fn deferred_prefix_in_deferred_for_body_is_recognized() {
    let mut fixture = Fixture::new(
        "let children = for _ in src.domain_validation_options {\n  mock.test.resource {\n    name_prefix = src.identifier\n  }\n}\n",
        SOURCE,
        false,
    );
    fixture.subject = "mock.test.resource.children[?]".into();
    assert_rejected(
        fixture,
        "'src.identifier'",
        "main.crn",
        Range::new(Position::new(2, 4), Position::new(2, 32)),
        false,
    )
    .await;
}
