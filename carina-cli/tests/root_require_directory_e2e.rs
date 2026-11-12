//! Root requirements use the same directory resolver as module requirements.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;

use carina_core::provider::loaded_provider_snapshot_from_factories;
use carina_core::schema::SchemaRegistry;
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockScope};
use serde_json::{Value, json};
use tower_lsp::lsp_types::{Position, Range};

struct Directory {
    _temp: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
}

impl Directory {
    fn new(condition: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let home = temp.path().join("home");
        fs::create_dir_all(project.join("checked")).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(
            project.join("providers.crn"),
            "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
        )
        .unwrap();
        fs::write(
            project.join("main.crn"),
            "let n = 2\nlet r = mock.test.resource { name = 'root' }\nlet checked = use { source = './checked' }\nlet control = checked { n = 2 }\n",
        ).unwrap();
        fs::write(
            project.join("checked/arguments.crn"),
            "arguments { n: Int }\n",
        )
        .unwrap();
        fs::write(
            project.join("checked/guards.crn"),
            "require n == 2, 'module guard'\n",
        )
        .unwrap();
        let directory = Self {
            _temp: temp,
            project,
            home,
        };
        directory.guard("true");
        success(&directory.run(&["init", "."]));
        directory.guard(condition);
        directory
    }

    fn guard(&self, condition: &str) {
        fs::write(
            self.project.join("guards.crn"),
            format!("// sibling guard\nrequire {condition}, 'root guard'\n"),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .args(args)
            .env("HOME", &self.home)
            .env("CARINA_TEST_HOME", &self.home)
            .env("CARINA_TRUST_RECORD_DIR", self.home.join(".carina/trust"))
            .env(
                "CARINA_PLUGIN_CACHE_DIR",
                self.home.join(".carina/plugin-cache"),
            )
            .env("CARINA_MOCK_STATE_FILE", self.home.join("mock-state.json"))
            .env("CARINA_ROOT_REQUIRE_KEY", "PRIVATE_ROOT_REQUIRE_MAP_KEY")
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap()
    }

    fn assert_validate_diagnostic(&self, condition: &str, message: &str) {
        let human = self.run(&["validate", "."]);
        rejected(&human, message);
        let text = output_text(&human);
        assert!(text.contains("guards.crn"), "{text}");
        let output = self.run(&["validate", "--json", "."]);
        assert!(!output.status.success(), "{}", output_text(&output));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        assert_eq!(errors[0]["message"], message);
        assert_eq!(errors[0]["location"]["file"], "guards.crn");
        assert_eq!(
            errors[0]["location"]["range"],
            json!({
                "start": {"line": 2, "column": 9},
                "end": {"line": 2, "column": 9 + condition.len()}
            })
        );

        let snapshot = Arc::new(loaded_provider_snapshot_from_factories(
            Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
                MockBehavior::default(),
                MockScope::memory(),
            ))]),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        ));
        let engine = DiagnosticEngine::new(snapshot.clone(), vec!["mock".into()]);
        // Analyzing any sibling reads the whole directory. The error belongs
        // only to guards.crn, regardless of which sibling was analyzed first.
        for name in ["main.crn", "providers.crn", "guards.crn"] {
            let path = self.project.join(name);
            let document = Document::new_with_path(
                fs::read_to_string(&path).unwrap(),
                path.clone(),
                Arc::new(snapshot.provider_context().as_provider_context().clone()),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            if name == "guards.crn" {
                let matching: Vec<_> = diagnostics
                    .iter()
                    .filter(|d| d.message == message)
                    .collect();
                assert_eq!(matching.len(), 1, "{diagnostics:?}");
                assert_eq!(
                    matching[0].range,
                    Range::new(
                        Position::new(1, 8),
                        Position::new(1, 8 + condition.len() as u32)
                    )
                );
            } else {
                assert!(
                    diagnostics.iter().all(|d| d.message != message),
                    "{name}: {diagnostics:?}"
                );
            }
        }
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
}

fn rejected(output: &Output, message: &str) {
    let text = output_text(output);
    assert!(!output.status.success(), "unexpected success: {text}");
    assert!(text.contains(message), "{text}");
    assert!(!text.contains("Declare it with 'let'"), "{text}");
}

#[test]
fn root_require_false_validate_cli_lsp_parity() {
    Directory::new("1 == 2").assert_validate_diagnostic("1 == 2", "require failed: root guard");
}

#[test]
fn root_require_false_plan() {
    rejected(
        &Directory::new("1 == 2").run(&["plan", "."]),
        "require failed: root guard",
    );
}

#[test]
fn root_require_false_plan_out_does_not_write() {
    let directory = Directory::new("1 == 2");
    rejected(
        &directory.run(&["plan", "--out", "saved.plan", "."]),
        "require failed: root guard",
    );
    assert!(!directory.project.join("saved.plan").exists());
}

#[test]
fn root_require_false_live_apply_preserves_state() {
    let directory = Directory::new("1 == 2");
    rejected(
        &directory.run(&["apply", "--auto-approve", "."]),
        "require failed: root guard",
    );
    assert!(!directory.project.join("carina.state.json").exists());
    assert!(!directory.home.join("mock-state.json").exists());
    directory.guard("1 == 1");
    success(&directory.run(&["apply", "--auto-approve", "."]));
    let paths = [
        directory.project.join("carina.state.json"),
        directory.home.join("mock-state.json"),
    ];
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    directory.guard("1 == 2");
    rejected(
        &directory.run(&["apply", "--auto-approve", "."]),
        "require failed: root guard",
    );
    assert_eq!(before, paths.each_ref().map(|path| fs::read(path).unwrap()));
}

#[test]
fn root_require_true_paths_and_saved_plan_contract() {
    let directory = Directory::new("false");
    rejected(
        &directory.run(&["plan", "--out", "saved.plan", "."]),
        "require failed: root guard",
    );
    assert!(!directory.project.join("saved.plan").exists());
    directory.guard("true");
    for args in [
        &["validate", "."][..],
        &["validate", "--json", "."],
        &["plan", "."],
        &["plan", "--out", "saved.plan", "."],
    ] {
        success(&directory.run(args));
    }
    let plan_path = directory.project.join("saved.plan");
    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    assert_eq!(plan["version"], 45);
    assert!(!plan.to_string().contains("root guard"));
    // Saved apply consumes the validated snapshot and does not reread guards.
    directory.guard("false");
    let mut old = plan.clone();
    old["version"] = json!(44);
    fs::write(
        directory.project.join("old.plan"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    rejected(
        &directory.run(&["apply", "--auto-approve", "old.plan"]),
        "Unsupported plan file version: 44 (expected 45)",
    );
    assert!(!directory.project.join("carina.state.json").exists());
    success(&directory.run(&["apply", "--auto-approve", "saved.plan"]));
    directory.guard("true");
    success(&directory.run(&["apply", "--auto-approve", "."]));
}

macro_rules! unknown_name {
    ($test:ident, $condition:expr, $name:expr) => {
        #[test]
        fn $test() {
            Directory::new($condition).assert_validate_diagnostic($condition, &format!("require failed: root guard; error evaluating constraint: unknown variable '{}' in constraint expression", $name));
        }
    };
}
unknown_name!(root_require_unknown_let, "n == 2", "n");
unknown_name!(root_require_unknown_resource, "r.identifier != null", "r");
unknown_name!(root_require_unknown_undeclared, "missing == 2", "missing");
unknown_name!(root_require_unknown_index, "n[0] == 2", "n");
unknown_name!(root_require_unknown_map_key, "r['key'] == 2", "r");
unknown_name!(
    root_require_unknown_even_when_short_circuited,
    "true || missing == 2",
    "missing"
);

macro_rules! expression {
    ($test:ident, $condition:expr) => {
        #[test]
        fn $test() {
            let directory = Directory::new($condition);
            success(&directory.run(&["validate", "."]));
            directory.guard(&format!("!({})", $condition));
            rejected(
                &directory.run(&["validate", "."]),
                "require failed: root guard",
            );
        }
    };
}
expression!(
    root_require_expr_boolean,
    "true && !false || env('unused') == 1"
);
expression!(
    root_require_expr_integer,
    "-2 < 1 && 2 <= 2 && 3 >= 2 && 3 > 2 && 2 != 1 && 1 == 1"
);
expression!(root_require_expr_float, "1.5 < 2 && -2.5 != 1.5");
expression!(root_require_expr_duration, "1min == 60s && 2h > 1h");
expression!(root_require_expr_string, "'a' == \"a\" && 'a' != 'b'");
expression!(
    root_require_expr_arithmetic,
    "(2 + 3) * 4 - 6 / 2 == 17 && 7 % 3 == 1"
);
expression!(root_require_expr_null, "null == null && 'value' != null");
expression!(root_require_expr_len, "len('abc') == 3");
expression!(root_require_expr_length, "length(\"a\\nb\") == 4");
expression!(
    root_require_expr_unicode_length_is_bytes,
    "length('あ') == 3"
);

#[test]
fn root_require_rejects_general_builtins_and_user_functions() {
    for function in ["env", "coalesce", "custom"] {
        let condition = format!("{function}('unused') == 1");
        let directory = Directory::new(&condition);
        fs::write(
            directory.project.join("functions.crn"),
            "fn custom(value: String): Int { 1 }\n",
        )
        .unwrap();
        directory.assert_validate_diagnostic(&condition, &format!("require failed: root guard; error evaluating constraint: unknown function '{function}' in constraint expression"));
    }
}

#[test]
fn root_require_index_builtin_does_not_expose_map_key() {
    let directory = Directory::new("false");
    rejected(
        &directory.run(&["validate", "."]),
        "require failed: root guard",
    );
    directory.guard("r[env('CARINA_ROOT_REQUIRE_KEY')] != null");
    for args in [
        &["validate", "."][..],
        &["validate", "--json", "."],
        &["plan", "."],
        &["apply", "--auto-approve", "."],
    ] {
        let output = directory.run(args);
        rejected(
            &output,
            "index access key must be an integer or string, got function call",
        );
        assert!(!output_text(&output).contains("PRIVATE_ROOT_REQUIRE_MAP_KEY"));
    }
}

#[test]
fn root_require_evaluation_errors_are_decided_during_validate() {
    for (condition, detail) in [
        (
            "null",
            "constraint expression must return a boolean, got null",
        ),
        ("1", "constraint expression must return a boolean, got int"),
        (
            "length(null) == 0",
            "length() argument must be a string, list, or map, got null",
        ),
        (
            "len(1) == 1",
            "len() argument must be a string, list, or map, got int",
        ),
        ("length() == 0", "length() expects 1 argument, got 0"),
        ("1.5 + 1 == 2", "left operand of + must be int, got float"),
    ] {
        let directory = Directory::new(condition);
        rejected(&directory.run(&["validate", "."]), detail);
    }
    for condition in ["[1] == [1]", "{ key = 1 } == null", "'abc'[0] == 'a'"] {
        let directory = Directory::new(condition);
        let output = directory.run(&["validate", "."]);
        assert!(!output.status.success(), "{}", output_text(&output));
    }
}

#[test]
fn root_require_module_control_is_unchanged() {
    let directory = Directory::new("false");
    rejected(
        &directory.run(&["validate", "."]),
        "require failed: root guard",
    );
    directory.guard("true");
    fs::write(
        directory.project.join("checked/guards.crn"),
        "require n == 3, 'module guard'\n",
    )
    .unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["errors"][0]["message"],
        "module 'checked' (call 'control'): arguments 'n': module guard (got n = 2)"
    );
    assert_eq!(report["errors"][0]["location"]["file"], "main.crn");
    assert_eq!(report["errors"][0]["location"]["range"]["start"]["line"], 4);
    // Preserve the module template's existing static checks as well as its
    // instance evaluator diagnostics, including their authored ranges.
    for (condition, message, start, end) in [
        (
            "len('abc') > 0",
            "missing expression evidence: function declaration or builtin signature",
            9,
            18,
        ),
        (
            "custom('unused') == 1",
            "missing expression evidence: function declaration or builtin signature",
            9,
            24,
        ),
        (
            "1.5 + 1 == 2",
            "operator '+': left operand must be Int, got Float",
            13,
            14,
        ),
    ] {
        fs::write(
            directory.project.join("checked/guards.crn"),
            format!("require {condition}, 'module guard'\n"),
        )
        .unwrap();
        let output = directory.run(&["validate", "--json", "."]);
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["errors"][0]["message"], message);
        assert_eq!(
            report["errors"][0]["location"]["file"],
            "checked/guards.crn"
        );
        assert_eq!(
            report["errors"][0]["location"]["range"],
            json!({
                "start": {"line": 1, "column": start},
                "end": {"line": 1, "column": end}
            })
        );
    }
}
