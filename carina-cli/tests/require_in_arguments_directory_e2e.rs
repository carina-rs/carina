//! Directory-scoped argument requirements and their CLI/LSP contract.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::Arc,
};

use carina_core::{provider::loaded_provider_snapshot_from_factories, schema::SchemaRegistry};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockScope};
use serde_json::{Value, json};

struct Directory {
    _temp: tempfile::TempDir,
    path: PathBuf,
    home: PathBuf,
}

impl Directory {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("project");
        let home = temp.path().join("home");
        fs::create_dir_all(path.join("checked")).unwrap();
        fs::create_dir(&home).unwrap();
        let directory = Self {
            _temp: temp,
            path,
            home,
        };
        directory.write(
            "providers.crn",
            "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
        );
        directory.write(
            "main.crn",
            "let checked = use { source = './checked' }\nlet control = checked { n = 2 }\n",
        );
        directory.write("checked/arguments.crn", "arguments { n: Int }\n");
        directory
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.path.join(name), source).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.path)
            .args(args)
            .env("HOME", &self.home)
            .env("CARINA_TEST_HOME", &self.home)
            .env("CARINA_TRUST_RECORD_DIR", self.home.join(".carina/trust"))
            .env(
                "CARINA_PLUGIN_CACHE_DIR",
                self.home.join(".carina/plugin-cache"),
            )
            .env("CARINA_MOCK_STATE_FILE", self.home.join("mock-state.json"))
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap()
    }

    fn diagnostics(&self, name: &str) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let snapshot = Arc::new(loaded_provider_snapshot_from_factories(
            Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
                MockBehavior::default(),
                MockScope::memory(),
            ))]),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        ));
        let path = self.path.join(name);
        let document = Document::new_with_path(
            fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        DiagnosticEngine::new(snapshot, vec!["mock".into()]).analyze(&document, Some(&path))
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn issue4398_module_guard_cli_json_lsp_plan_parity() {
    let d = Directory::new();
    d.write(
        "checked/guards.crn",
        "arguments {\nrequire n == 3, 'module guard'\n}\n",
    );
    // Measured on main, with identical caller text. The moved declaration
    // preserves its condition coordinates; every consumer anchors at the call.
    let message = "module 'checked' (call 'control'): arguments 'n': module guard (got n = 2)";
    for args in [&["validate", "."][..], &["plan", "."]] {
        let output = d.run(args);
        assert!(!output.status.success(), "{}", text(&output));
        assert!(text(&output).contains(message), "{}", text(&output));
    }
    let output = d.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["errors"][0]["message"], message);
    assert_eq!(
        report["errors"][0]["location"],
        json!({"file":"main.crn", "range":{
        "start":{"line":2,"column":15},"end":{"line":2,"column":32}}})
    );
    let diagnostics = d.diagnostics("main.crn");
    let finding = diagnostics
        .iter()
        .find(|d| d.message == message)
        .unwrap_or_else(|| panic!("{diagnostics:?}"));
    assert_eq!(
        finding.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 14),
            tower_lsp::lsp_types::Position::new(1, 31)
        )
    );
}

#[test]
fn issue4398_require_reads_sibling_arguments() {
    for reverse in [false, true] {
        let d = Directory::new();
        fs::remove_file(d.path.join("checked/arguments.crn")).unwrap();
        let files = if reverse {
            ["z.crn", "a.crn"]
        } else {
            ["a.crn", "z.crn"]
        };
        d.write(
            &format!("checked/{}", files[0]),
            "arguments { require n == m, 'same' n: Int require n == m, 'same' }\n",
        );
        d.write(
            &format!("checked/{}", files[1]),
            "arguments { require n == m, 'same' m: Int require n == m, 'same' }\n",
        );
        d.write(
            "main.crn",
            "let checked = use { source = './checked' }\nlet control = checked { n = 2 m = 2 }\n",
        );
        let output = d.run(&["validate", "--json", "."]);
        assert!(output.status.success(), "{}", text(&output));
        assert!(d.diagnostics("main.crn").is_empty());
    }
}

#[test]
fn issue4398_module_require_static_diagnostics_unchanged() {
    let d = Directory::new();
    // These messages and columns were captured by main's module control.
    // Moving the require onto line 2 changes only its physical line.
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
        d.write(
            "checked/guards.crn",
            &format!("arguments {{\nrequire {condition}, 'module guard'\n}}\n"),
        );
        let output = d.run(&["validate", "--json", "."]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["errors"][0]["message"], message, "{report}");
        assert_eq!(
            report["errors"][0]["location"],
            json!({"file":"checked/guards.crn", "range":{
            "start":{"line":2,"column":start},"end":{"line":2,"column":end}}})
        );
    }
}

#[test]
fn issue4398_direct_module_validation_does_not_evaluate() {
    let d = Directory::new();
    d.write(
        "checked/guards.crn",
        "arguments { require false, 'template' require n > 0, 'positive' }\n",
    );
    for args in [
        &["validate", "checked"][..],
        &["validate", "--json", "checked"],
    ] {
        let output = d.run(args);
        assert!(output.status.success(), "{}", text(&output));
    }
    assert!(d.diagnostics("checked/guards.crn").is_empty());
    fs::remove_file(d.path.join("checked/arguments.crn")).unwrap();
    d.write("checked/guards.crn", "arguments { require 1 == 2, 'g' }\n");
    let output = d.run(&["validate", "--json", "checked"]);
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn issue4398_root_arguments_block_placement() {
    let message =
        "arguments blocks are only valid inside module definitions, not in root configurations.";
    for root in [
        "provider mock {}\n",
        "backend local { path = 'carina.state.json' }\n",
    ] {
        for arguments in ["arguments {}\n", "arguments { require false, 'g' }\n"] {
            let d = Directory::new();
            d.write("main.crn", "");
            d.write("providers.crn", root);
            d.write("arguments.crn", arguments);
            // Measured main CLI accepts an empty block; main LSP already
            // emits this error at the arguments keyword, range 0:0–0:9.
            let output = d.run(&["validate", "--json", "."]);
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["errors"][0]["message"], message, "{report}");
            let diagnostics = d.diagnostics("arguments.crn");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].message, message);
            assert_eq!(
                diagnostics[0].range,
                tower_lsp::lsp_types::Range::new(
                    tower_lsp::lsp_types::Position::new(0, 0),
                    tower_lsp::lsp_types::Position::new(0, 9)
                )
            );
        }
    }
    let d = Directory::new();
    d.write("main.crn", "");
    d.write("arguments.crn", "arguments { n: Int }\n");
    let output = d.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["errors"][0]["message"],
        "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
    );
}

const REQUIRE_PLACEMENT_ERROR: &str = "`require` must be written inside an `arguments` block";

fn assert_legacy_syntax_error(name: &str, source: &str, direct_module: bool, line: u32) {
    let d = Directory::new();
    d.write(name, source);
    let input = if direct_module { "checked" } else { "." };
    for command in ["validate", "plan"] {
        let output = d.run(&[command, input]);
        let rendered = text(&output);
        assert!(!output.status.success(), "{rendered}");
        assert!(rendered.contains(REQUIRE_PLACEMENT_ERROR), "{rendered}");
        assert!(rendered.contains(&format!("{line}:1")), "{rendered}");
        if name.starts_with("checked/") && !direct_module {
            assert!(
                rendered.contains("Parse error in checked/guards.crn:"),
                "{rendered}"
            );
        }
    }
    let output = d.run(&["validate", "--json", input]);
    assert!(!output.status.success(), "{}", text(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["errors"][0]["message"], REQUIRE_PLACEMENT_ERROR);
    assert_eq!(
        report["errors"][0]["location"],
        json!({
            "file": name,
            "range": {"start": {"line":line,"column":1}, "end": {"line":line,"column":8}}
        })
    );
    let findings = d.diagnostics(name);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].message, REQUIRE_PLACEMENT_ERROR);
    assert_eq!(
        findings[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(line - 1, 0),
            tower_lsp::lsp_types::Position::new(line - 1, 7)
        )
    );
    for other in ["main.crn", "providers.crn", "checked/arguments.crn"] {
        let other_findings = d.diagnostics(other);
        assert!(
            other_findings
                .iter()
                .all(|finding| !finding.message.contains(REQUIRE_PLACEMENT_ERROR)),
            "{other}: {other_findings:?}"
        );
    }
    let output = d.run(&[
        "fmt",
        if name.starts_with("checked/") {
            "checked"
        } else {
            "."
        },
    ]);
    assert!(!output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains(&format!(
            "Parse error at {line}:1: {REQUIRE_PLACEMENT_ERROR}"
        )),
        "{}",
        text(&output)
    );
    assert_eq!(fs::read_to_string(d.path.join(name)).unwrap(), source);
}

#[test]
fn issue4398_root_sibling_top_level_require_is_syntax_error() {
    assert_legacy_syntax_error("guards.crn", "require n == 2, 'x'\n", false, 1);
}

#[test]
fn issue4398_module_top_level_require_is_syntax_error() {
    assert_legacy_syntax_error("checked/guards.crn", "require n == 2, 'x'\n", false, 1);
}

#[test]
fn issue4398_direct_module_top_level_require_is_syntax_error() {
    assert_legacy_syntax_error("checked/guards.crn", "require n == 2, 'x'\n", true, 1);
}

#[test]
fn issue4398_require_after_arguments_is_syntax_error() {
    assert_legacy_syntax_error(
        "guards.crn",
        "arguments {}\nrequire n == 2, 'x'\n",
        false,
        2,
    );
}

#[test]
fn issue4398_require_after_let_is_syntax_error() {
    assert_legacy_syntax_error("guards.crn", "let n = 2\nrequire n == 2, 'x'\n", false, 2);
}

fn assert_upstream_syntax_error(condition: &str) {
    let d = Directory::new();
    fs::create_dir(d.path.join("upstream")).unwrap();
    d.write(
        "upstream/exports.crn",
        "exports { value: String = 'published' }\n",
    );
    d.write(
        "upstream/guards.crn",
        &format!("// upstream guard\nrequire {condition}, 'upstream guard'\n"),
    );
    d.write(
        "main.crn",
        "let prior = upstream_state { source = './upstream' }\n",
    );
    let output = d.run(&["validate", "--json", "."]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    let finding = errors
        .iter()
        .find(|e| {
            e["location"]["file"]
                .as_str()
                .is_some_and(|f| f.ends_with("upstream/guards.crn"))
        })
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(finding["message"], REQUIRE_PLACEMENT_ERROR, "{report}");
    assert_eq!(
        finding["location"]["range"],
        json!({
            "start": {"line":2,"column":1}, "end": {"line":2,"column":8}
        })
    );
    for command in ["validate", "plan"] {
        let output = d.run(&[command, "."]);
        assert!(!output.status.success(), "{}", text(&output));
        assert!(
            text(&output).contains(REQUIRE_PLACEMENT_ERROR),
            "{}",
            text(&output)
        );
        assert!(text(&output).contains("2:1"), "{}", text(&output));
    }
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let base = carina_core::paths::ResolvedPath::new(&d.path).unwrap();
    let parsed = carina_core::config_loader::parse_directory(&base, &context).unwrap();
    let tree = carina_core::parser::AnalysisTree::try_from_file(parsed.parsed()).unwrap();
    carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
        let (_, failures) = carina_core::upstream_exports::resolve_upstream_exports_report(
            &base, &tree, session, None,
        )
        .unwrap();
        assert_eq!(failures.len(), 1);
        let failure = failures[0].detail();
        let cause = failure.parse_failure().unwrap();
        assert!(
            cause
                .source_file()
                .unwrap()
                .ends_with("upstream/guards.crn")
        );
        assert!(
            cause
                .parse_error()
                .unwrap()
                .diagnostic_message()
                .contains(REQUIRE_PLACEMENT_ERROR)
        );
        assert_eq!(
            carina_cli::error::format_error_with_source_chain(failure),
            failure.to_string()
        );
    });
}

#[test]
fn issue4398_upstream_false_require_keeps_syntax_location() {
    assert_upstream_syntax_error("false");
}

#[test]
fn issue4398_upstream_unknown_require_keeps_syntax_location() {
    assert_upstream_syntax_error("missing == 1");
}

#[test]
fn upstream_module_require_error_chain_renders_once() {
    let d = Directory::new();
    fs::create_dir_all(d.path.join("upstream/checked")).unwrap();
    d.write(
        "main.crn",
        "let prior = upstream_state { source = './upstream' }\n",
    );
    d.write("upstream/main.crn", "let checked = use { source = './checked' }\nlet control = checked { n = 2 }\nexports { value: Int = 1 }\n");
    d.write(
        "upstream/checked/arguments.crn",
        "arguments { n: Int require n == 3, 'upstream module guard' }\n",
    );
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let base = carina_core::paths::ResolvedPath::new(&d.path).unwrap();
    let parsed = carina_core::config_loader::parse_directory(&base, &context).unwrap();
    let tree = carina_core::parser::AnalysisTree::try_from_file(parsed.parsed()).unwrap();
    carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
        let (_, failures) = carina_core::upstream_exports::resolve_upstream_exports_report(
            &base, &tree, session, None,
        )
        .unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        let failure = failures[0].detail();
        let (source, cause) = failure
            .module_failure()
            .expect("typed upstream module failure");
        assert!(source.ends_with("upstream"));
        assert!(
            matches!(
                cause.root_cause(),
                carina_core::module_resolver::ModuleError::Constraint(_)
            ),
            "{cause:?}"
        );
        let rendered = carina_cli::error::format_error_with_source_chain(failure);
        assert_eq!(rendered, failure.to_string());
        assert_eq!(
            rendered.matches("upstream_state `prior`").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered
                .matches("module 'checked' (call 'control')")
                .count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("upstream module guard").count(),
            1,
            "{rendered}"
        );
    });
}

#[test]
fn issue4398_saved_plan_module_source_policy_unchanged() {
    for has_secret in [false, true] {
        let d = Directory::new();
        d.write(
            "checked/guards.crn",
            "arguments { require n == 2, 'module guard' }\n",
        );
        let comment = if has_secret {
            "comment = secret('ROUND1_SECRET_VALUE')"
        } else {
            ""
        };
        d.write("main.crn", &format!("let r = mock.test.resource {{ name = 'root' {comment} }}\nlet checked = use {{ source = './checked' }}\nlet control = checked {{ n = 2 }}\n"));
        let init = d.run(&["init", "."]);
        assert!(init.status.success(), "{}", text(&init));
        let plan = d.run(&["plan", "--out", "saved.plan", "."]);
        assert!(plan.status.success(), "{}", text(&plan));
        d.write(
            "checked/guards.crn",
            "arguments { require n == 3, 'module guard' }\n",
        );
        let replay = d.run(&["apply", "--auto-approve", "saved.plan"]);
        if has_secret {
            assert!(!replay.status.success(), "{}", text(&replay));
            assert!(
                text(&replay).contains("Saved-plan secret verification failed"),
                "{}",
                text(&replay)
            );
            assert!(!d.path.join("carina.state.json").exists());
        } else {
            assert!(replay.status.success(), "{}", text(&replay));
        }
        assert!(!text(&replay).contains("ROUND1_SECRET_VALUE"));
    }
}

#[test]
fn issue4398_pending_constraint_wire_unchanged() {
    let d = Directory::new();
    fs::remove_file(d.path.join("checked/arguments.crn")).unwrap();
    // Initialize before adding the module, whose arguments are unresolved.
    d.write("main.crn", "");
    let initialized = d.run(&["init", "."]);
    assert!(initialized.status.success(), "{}", text(&initialized));
    d.write("main.crn", "let producer = mock.test.resource { name = 'producer' }\nlet checked = use { source = './checked' }\nlet control = checked { before = producer.identifier after = producer.identifier }\n");
    d.write("checked/a.crn", "arguments { require before == after, 'a0' before: String require after == before, 'a1' }\n");
    d.write(
        "checked/z.crn",
        "arguments { require after == before, 'z0' after: String require before == after, 'z1' }\n",
    );
    let output = d.run(&["plan", "--out", "saved.plan", "."]);
    assert!(output.status.success(), "{}", text(&output));
    let bytes = fs::read(d.path.join("saved.plan")).unwrap();
    let plan: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(plan["version"], 46);
    // Captured from main-carina (83fec2e1c), with the same caller and four
    // legacy top-level declarations in file order, then source order.
    let bound_argument = |name| {
        json!({"BoundArgument": {
            "argument": name, "operand": name,
            "value": {"ResourceRef": {"path": {"binding": "producer", "attribute": "identifier"}}}
        }})
    };
    let expected = Value::Array([
        ("before", "after", "'a0'"),
        ("after", "before", "'a1'"),
        ("after", "before", "'z0'"),
        ("before", "after", "'z1'"),
    ].into_iter().enumerate().map(|(index, (lhs, rhs, message))| json!({
        "id": format!("require:{index}"),
        "expression": {"Compare": {"lhs": bound_argument(lhs), "op": "Eq", "rhs": bound_argument(rhs)}},
        "message": message,
    })).collect());
    assert_eq!(plan["compositions"][0]["pending_constraints"], expected);
    let _: carina_cli::commands::plan::PlanFile = serde_json::from_slice(&bytes).unwrap();
    let replay = d.run(&["apply", "--auto-approve", "--lock=false", "saved.plan"]);
    assert!(replay.status.success(), "{}", text(&replay));
}
