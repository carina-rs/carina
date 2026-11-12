//! Directory-scoped regressions for retained validation strings (carina#4386).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use carina_core::provider::loaded_provider_snapshot_from_factories;
use carina_core::schema::SchemaRegistry;
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockScope};
use serde_json::{Value, json};
use tower_lsp::lsp_types::{Position, Range};

const CANARY: &str = "CANARY_4386";
const INTERPOLATION: &str = "${secret('CANARY_4386')}";

struct Fixture {
    _temp: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    authored: BTreeMap<PathBuf, Vec<u8>>,
}

fn files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

impl Fixture {
    fn new(root: &str, module: Option<&str>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let home = temp.path().join("home");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(
            project.join("providers.crn"),
            "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
        )
        .unwrap();
        let mut fixture = Self {
            _temp: temp,
            project,
            home,
            authored: BTreeMap::new(),
        };
        let init = fixture.run(&["init", "."]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        fs::write(
            fixture.project.join("sibling.crn"),
            "let sibling = 'unchanged'\n",
        )
        .unwrap();
        let mut main = "let vpc = mock.test.resource { name = 'vpc' }\n".to_string();
        if let Some(module) = module {
            fs::create_dir(fixture.project.join("checked")).unwrap();
            fs::write(fixture.project.join("checked/main.crn"), module).unwrap();
            main.push_str("let checked = use { source = './checked' }\nchecked { r = vpc }\n");
        }
        main.push_str(root);
        fs::write(fixture.project.join("main.crn"), main).unwrap();
        fixture.authored = files(&fixture.project)
            .into_iter()
            .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "crn"))
            .collect();
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(args)
            .current_dir(&self.project)
            .env("PWD", &self.project)
            .env("HOME", &self.home)
            .env("CARINA_TEST_HOME", &self.home)
            .env("CARINA_TRUST_RECORD_DIR", self.home.join(".carina/trust"))
            .env(
                "CARINA_PLUGIN_CACHE_DIR",
                self.home.join(".carina/plugin-cache"),
            )
            .env("NO_COLOR", "1")
            .env("CARINA_WASI_HTTP_TRACE", "1")
            .env("CARINA_MOCK_STATE_FILE", self.home.join("mock-state.json"))
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap();
        assert!(
            !output
                .stdout
                .windows(CANARY.len())
                .any(|w| w == CANARY.as_bytes())
        );
        assert!(
            !output
                .stderr
                .windows(CANARY.len())
                .any(|w| w == CANARY.as_bytes())
        );
        output
    }

    fn assert_no_written_canary(&self) {
        // Authored inputs already contain the probe. Inspect every newly written
        // or changed file under both the project and HOME, without extension filters.
        for (path, bytes) in files(&self.project).into_iter().chain(files(&self.home)) {
            if self.authored.get(&path) == Some(&bytes) {
                continue;
            }
            assert!(
                !bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
                "plaintext in {}",
                path.display()
            );
        }
        for (path, bytes) in &self.authored {
            assert_eq!(&fs::read(path).unwrap(), bytes, "authored input changed");
        }
    }

    fn assert_rejected(&self, name: &str, position: &str) {
        let message = format!("interpolation is not supported in a {position}");
        let source = fs::read_to_string(self.project.join(name)).unwrap();
        let offset = source.find(INTERPOLATION).unwrap();
        let line = source[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
        let column = source[..offset]
            .rsplit('\n')
            .next()
            .unwrap()
            .chars()
            .count()
            + 1;
        let human = self.run(&["validate", "."]);
        let text = String::from_utf8_lossy(&human.stderr);
        assert!(!human.status.success());
        assert!(text.contains(name), "{text}");
        assert!(
            text.contains(&format!("line {line}, column {column}")),
            "{text}"
        );
        assert!(text.contains(&message), "{text}");

        let output = self.run(&["validate", "--json", "."]);
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let diagnostic = report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["message"] == message)
            .unwrap();
        assert_eq!(diagnostic["location"]["file"], name);
        assert_eq!(
            diagnostic["location"]["range"],
            json!({
                "start": {"line": line, "column": column},
                "end": {"line": line, "column": column + INTERPOLATION.len()}
            })
        );
        assert!(
            !self
                .run(&["plan", "--out", "plan.bin", "."])
                .status
                .success()
        );
        let plan = self.project.join("plan.bin");
        if plan.exists() {
            let object: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
            assert!(!object.to_string().contains(CANARY));
        }
        assert!(
            !plan.exists(),
            "a rejected condition must not produce a saved plan"
        );
        assert!(!self.run(&["apply", "--auto-approve", "."]).status.success());
        self.assert_no_written_canary();

        let snapshot = Arc::new(loaded_provider_snapshot_from_factories(
            Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
                MockBehavior::default(),
                MockScope::memory(),
            ))]),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        ));
        let engine = DiagnosticEngine::new(snapshot.clone(), vec!["mock".into()]);
        let expected_range = Range::new(
            Position::new(line as u32 - 1, column as u32 - 1),
            Position::new(line as u32 - 1, (column + INTERPOLATION.len()) as u32 - 1),
        );
        for file in [
            "providers.crn",
            "main.crn",
            "sibling.crn",
            "checked/main.crn",
        ] {
            let path = self.project.join(file);
            if !path.exists() {
                continue;
            }
            let doc = Document::new_with_path(
                fs::read_to_string(&path).unwrap(),
                path.clone(),
                Arc::new(snapshot.provider_context().as_provider_context().clone()),
            );
            let diagnostics = engine.analyze(&doc, Some(&path));
            assert!(!format!("{diagnostics:?}").contains(CANARY));
            let matching: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message == message)
                .collect();
            if file == name {
                assert_eq!(matching.len(), 1, "{file}: {diagnostics:?}");
                assert_eq!(matching[0].range, expected_range);
            } else {
                assert!(matching.is_empty(), "sibling error misattributed to {file}");
            }
        }
    }
}

#[test]
fn root_require_condition_rejects_interpolation() {
    Fixture::new(r#"require "${secret('CANARY_4386')}" == 'x', 'bad'"#, None)
        .assert_rejected("main.crn", "`require` condition");
}

#[test]
fn module_require_condition_rejects_interpolation() {
    Fixture::new("", Some("arguments { r: mock.test.resource }\nrequire r.identifier != \"${secret('CANARY_4386')}\", 'bad identifier'\nmock.test.resource { name = 'consumer' parent = r.identifier }\n"))
        .assert_rejected("checked/main.crn", "`require` condition");
}

#[test]
fn validation_condition_rejects_interpolation() {
    Fixture::new("", Some(r#"arguments { r: mock.test.resource { validation { condition = r.identifier != "${secret('CANARY_4386')}" error_message = 'bad' } } }"#))
        .assert_rejected("checked/main.crn", "validation condition");
}

#[test]
fn validation_condition_without_message_rejects_interpolation() {
    Fixture::new("", Some(r#"arguments { r: mock.test.resource { validation { condition = r.identifier != "${secret('CANARY_4386')}" } } }"#))
        .assert_rejected("checked/main.crn", "validation condition");
}

#[test]
fn constraint_function_argument_rejects_interpolation() {
    Fixture::new(
        "",
        Some(
            r#"arguments { r: mock.test.resource }
require length("${secret('CANARY_4386')}") > 0 && r.identifier != 'x', 'bad'"#,
        ),
    )
    .assert_rejected("checked/main.crn", "`require` condition");
}

#[test]
fn root_require_message_rejects_interpolation() {
    Fixture::new(r#"require false, "${secret('CANARY_4386')}""#, None)
        .assert_rejected("main.crn", "`require` message");
}

#[test]
fn module_require_message_rejects_interpolation() {
    Fixture::new(
        "",
        Some(
            r#"arguments { r: mock.test.resource }
require r.identifier == 'impossible', "${secret('CANARY_4386')}""#,
        ),
    )
    .assert_rejected("checked/main.crn", "`require` message");
}

#[test]
fn validation_error_message_keeps_secret_protection() {
    let fixture = Fixture::new(
        "",
        Some(
            r#"arguments { r: mock.test.resource { validation {
condition = r.identifier == 'impossible'
error_message = "${secret('CANARY_4386')}"
} } }"#,
        ),
    );
    assert!(fixture.run(&["validate", "."]).status.success());
    assert!(
        fixture
            .run(&["plan", "--out", "plan.bin", "."])
            .status
            .success()
    );
    let object: Value =
        serde_json::from_slice(&fs::read(fixture.project.join("plan.bin")).unwrap()).unwrap();
    assert!(!object.to_string().contains(CANARY));
    let apply = fixture.run(&["apply", "--auto-approve", "--lock=false", "plan.bin"]);
    assert!(!apply.status.success());
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(
        rendered.contains("validation failed for argument 'r'"),
        "{rendered}"
    );
    fixture.assert_no_written_canary();
}

#[test]
fn require_without_message_remains_a_safe_syntax_error() {
    for module in [false, true] {
        let source = "require \"${secret('CANARY_4386')}\" == 'x'\n";
        let fixture = if module {
            Fixture::new("", Some(source))
        } else {
            Fixture::new(source, None)
        };
        for args in [
            &["validate", "."][..],
            &["validate", "--json", "."],
            &["plan", "--out", "plan.bin", "."],
        ] {
            assert!(!fixture.run(args).status.success());
        }
        fixture.assert_no_written_canary();
    }
}

#[test]
fn bound_argument_operand_rejects_secret_subscript_before_retaining_diagnostic_text() {
    let fixture = Fixture::new(
        "",
        Some(
            r#"arguments { r: mock.test.resource }
require r["${secret('CANARY_4386')}"] == 'x', 'bad operand'"#,
        ),
    );
    for args in [
        &["validate", "."][..],
        &["validate", "--json", "."],
        &["plan", "--out", "plan.bin", "."],
    ] {
        let output = fixture.run(args);
        assert!(!output.status.success());
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            rendered.contains("index access key must be an integer or string, got secret"),
            "{rendered}"
        );
    }
    assert!(!fixture.project.join("plan.bin").exists());
    fixture.assert_no_written_canary();
}

#[test]
fn literal_constraint_strings_survive_saved_plan_apply() {
    let fixture = Fixture::new(
        "",
        Some(
            r#"arguments { r: mock.test.resource }
require r.identifier != "plain", 'bad plain'
require r.identifier != '${secret("literal")}', 'bad single quote'
require r.identifier != "\${secret('literal')}", 'bad escaped dollar'
require r.identifier != "raw\nescapes\\", 'bad raw escapes'
mock.test.resource { name = 'consumer' parent = r.identifier }
"#,
        ),
    );
    assert!(fixture.run(&["validate", "."]).status.success());
    assert!(
        fixture
            .run(&["plan", "--out", "plan.bin", "."])
            .status
            .success()
    );
    let bytes = fs::read(fixture.project.join("plan.bin")).unwrap();
    let plan: carina_cli::commands::plan::PlanFile = serde_json::from_slice(&bytes).unwrap();
    let wire = serde_json::to_value(plan).unwrap();
    for (constraint, literal) in wire["compositions"][0]["pending_constraints"]
        .as_array()
        .unwrap()
        .iter()
        .zip([
            r#""plain""#,
            r#"'${secret("literal")}'"#,
            r#""\${secret('literal')}""#,
            r#""raw\nescapes\\""#,
        ])
    {
        assert_eq!(
            constraint["expression"]["Compare"]["rhs"]["String"],
            literal
        );
    }
    let apply = fixture.run(&["apply", "--auto-approve", "--lock=false", "plan.bin"]);
    assert!(
        apply.status.success(),
        "{}{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    fixture.assert_no_written_canary();
}
