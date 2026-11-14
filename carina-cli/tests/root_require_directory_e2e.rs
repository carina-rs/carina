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

#[test]
fn round5_directory_read_error_chain_renders_once() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("blocked.crn");
    fs::write(&file, [0xff]).unwrap();
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let error = carina_core::config_loader::parse_directory_for_metadata_report(
        &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context,
        &Default::default(),
        &SchemaRegistry::new(),
    )
    .unwrap_err();
    assert_eq!(error.source_file(), Some(file.as_path()));
    assert_eq!(
        error.io_error().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        error.to_string(),
        format!(
            "Failed to read {}: {}",
            file.display(),
            error.io_error().unwrap()
        )
    );
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(&error),
        error.to_string()
    );
}

#[test]
fn round5_directory_discovery_error_chain_renders_once() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let error = carina_core::config_loader::parse_directory_for_metadata_report(
        &carina_core::paths::ResolvedPath::new(missing).unwrap(),
        &context,
        &Default::default(),
        &SchemaRegistry::new(),
    )
    .unwrap_err();
    assert_eq!(
        error.io_error().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(&error),
        error.to_string()
    );
}

fn round5_upstream_failures() -> (
    carina_core::upstream_exports::UpstreamExportSourceError,
    carina_core::upstream_exports::UpstreamResolveFailure,
) {
    use carina_core::config_loader::parse_directory;
    use carina_core::paths::ResolvedPath;
    use carina_core::upstream_exports::{
        resolve_upstream_exports_from_parsed, resolve_upstream_exports_report,
    };
    let directory = tempfile::tempdir().unwrap();
    let upstream = directory.path().join("upstream");
    fs::create_dir(&upstream).unwrap();
    fs::write(
        upstream.join("guards.crn"),
        "require false, 'upstream guard'\n",
    )
    .unwrap();
    fs::write(
        upstream.join("exports.crn"),
        "exports { value: String = 'published' }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("main.crn"),
        "let prior = upstream_state { source = './upstream' }\n",
    )
    .unwrap();
    let base = ResolvedPath::new(directory.path()).unwrap();
    let upstream = ResolvedPath::new(upstream).unwrap();
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
        let parsed = parse_directory(&upstream, &context).unwrap();
        let source_error = resolve_upstream_exports_from_parsed(
            parsed,
            &upstream,
            session,
            None,
            &Default::default(),
        )
        .unwrap_err();
        let parsed = parse_directory(&base, &context).unwrap();
        let tree = carina_core::parser::AnalysisTree::try_from_file(parsed.parsed()).unwrap();
        let (_, errors) = resolve_upstream_exports_report(&base, &tree, session, None).unwrap();
        assert_eq!(errors.len(), 1);
        (
            source_error,
            errors.into_iter().next().unwrap().into_parts().1,
        )
    })
}

#[test]
fn round5_module_guard_error_chain_renders_once() {
    let (source_error, _) = round5_upstream_failures();
    let carina_core::upstream_exports::UpstreamExportSourceError::Module { error, .. } =
        source_error
    else {
        panic!("expected module failure");
    };
    let finding = error.directory_constraint().unwrap();
    assert!(finding.location().file().path().ends_with("guards.crn"));
    assert_eq!(finding.location().span().start_column(), 9);
    assert_eq!(finding.location().span().end_column(), 14);
    assert_eq!(
        finding.detail().to_string(),
        "require failed: upstream guard"
    );
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(error.as_ref()),
        error.to_string()
    );
}

#[test]
fn round5_upstream_source_error_chain_renders_once() {
    let (error, _) = round5_upstream_failures();
    let (base, module) = error.module_failure().unwrap();
    assert!(base.ends_with("upstream"));
    assert!(module.directory_constraint().is_some());
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(&error),
        error.to_string()
    );
}

#[test]
fn round5_upstream_report_error_chain_renders_once() {
    let (_, error) = round5_upstream_failures();
    assert!(error.module_failure().is_some());
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(&error),
        error.to_string()
    );
}

fn assert_round4_empty_template(blocks: &str) {
    let directory = Directory::new("true");
    fs::create_dir(directory.project.join("empty")).unwrap();
    fs::write(directory.project.join("empty/arguments.crn"), blocks).unwrap();
    fs::write(
        directory.project.join("empty/guards.crn"),
        "require false, 'guard'\n",
    )
    .unwrap();
    success(&directory.run(&["validate", "empty"]));
    success(&directory.run(&["validate", "--json", "empty"]));
    fs::write(
        directory.project.join("main.crn"),
        "let empty = use { source = './empty' }\nlet call = empty {}\n",
    )
    .unwrap();
    rejected(
        &directory.run(&["validate", "."]),
        "Module not found: ./empty",
    );
}

#[test]
fn round4_empty_arguments_validate_as_template() {
    assert_round4_empty_template("arguments {}\n");
}
#[test]
fn round4_empty_attributes_validate_as_template() {
    assert_round4_empty_template("attributes {}\n");
}
#[test]
fn round4_both_empty_blocks_validate_as_template() {
    assert_round4_empty_template("arguments {}\nattributes {}\n");
}

#[test]
fn round1_validate_standalone_module_directory_keeps_template_scope() {
    let directory = Directory::new("true");
    fs::write(
        directory.project.join("checked/guards.crn"),
        "require n > 0, 'positive'\n",
    )
    .unwrap();
    for args in [
        &["validate", "checked"][..],
        &["validate", "--json", "checked"],
    ] {
        success(&directory.run(args));
    }
}

fn assert_round1_upstream_guard(condition: &str) {
    let directory = Directory::new("true");
    let upstream = directory.project.join("upstream");
    fs::create_dir(&upstream).unwrap();
    fs::write(
        upstream.join("providers.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .unwrap();
    fs::write(
        upstream.join("exports.crn"),
        "exports { value: String = 'published' }\n",
    )
    .unwrap();
    fs::write(
        upstream.join("guards.crn"),
        format!("// upstream guard\nrequire {condition}, 'upstream guard'\n"),
    )
    .unwrap();
    fs::write(directory.project.join("main.crn"), "let upstream = upstream_state { source = './upstream' }\nlet r = mock.test.resource { name = 'root' comment = upstream.value }\n").unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.contains("require failed: upstream guard"))
        })
        .unwrap();
    let file = error["location"]["file"]
        .as_str()
        .expect("upstream require must retain its authored file");
    assert!(file.ends_with("upstream/guards.crn"), "{report}");
    assert_eq!(
        error["location"]["range"],
        json!({
            "start": {"line": 2, "column": 9},
            "end": {"line": 2, "column": 9 + condition.len()}
        })
    );
    let human = directory.run(&["validate", "."]);
    let text = output_text(&human);
    assert!(text.contains("upstream/guards.crn:2:9"), "{text}");
}

#[test]
fn round1_upstream_false_require_keeps_location() {
    assert_round1_upstream_guard("false");
}

#[test]
fn round1_upstream_unknown_require_keeps_location() {
    assert_round1_upstream_guard("missing == 1");
}

#[test]
fn round1_zero_module_calls_and_data_source_only_guards() {
    for main in [
        "let r = mock.test.resource { name = 'root' }\n",
        "let r = read mock.test.resource { name = 'root' }\n",
    ] {
        let directory = Directory::new("false");
        fs::write(directory.project.join("main.crn"), main).unwrap();
        rejected(
            &directory.run(&["validate", "."]),
            "require failed: root guard",
        );
        rejected(&directory.run(&["plan", "."]), "require failed: root guard");
    }
}

#[test]
fn round1_saved_plan_secret_verification_matches_module_policy() {
    for has_secret in [false, true] {
        for root_guard in [false, true] {
            let directory = Directory::new("true");
            let comment = if has_secret {
                "comment = secret('ROUND1_SECRET_VALUE')"
            } else {
                ""
            };
            fs::write(directory.project.join("main.crn"), format!(
                "let r = mock.test.resource {{ name = 'root' {comment} }}\nlet checked = use {{ source = './checked' }}\nlet control = checked {{ n = 2 }}\n"
            )).unwrap();
            success(&directory.run(&["plan", "--out", "saved.plan", "."]));
            let message = if root_guard {
                directory.guard("false");
                "require failed: root guard"
            } else {
                fs::write(
                    directory.project.join("checked/guards.crn"),
                    "require n > 2, 'module guard'\n",
                )
                .unwrap();
                "module 'checked' (call 'control'): arguments 'n': module guard (got n = 2)"
            };
            let output = directory.run(&["apply", "--auto-approve", "saved.plan"]);
            if has_secret {
                assert!(
                    !output.status.success(),
                    "{message}: {}",
                    output_text(&output)
                );
                assert!(output_text(&output).contains("Saved-plan secret verification failed"));
                assert!(!directory.project.join("carina.state.json").exists());
            } else {
                success(&output);
            }
            assert!(!output_text(&output).contains("ROUND1_SECRET_VALUE"));
        }
    }
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
    let current_version = carina_cli::commands::plan::PlanFile::CURRENT_VERSION;
    let previous_version = current_version - 1;
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
    assert_eq!(plan["version"], current_version);
    assert!(!plan.to_string().contains("root guard"));
    // This secret-free plan reloads backend syntax without rechecking guards.
    directory.guard("false");
    let mut old = plan.clone();
    old["version"] = json!(previous_version);
    fs::write(
        directory.project.join("old.plan"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    rejected(
        &directory.run(&["apply", "--auto-approve", "old.plan"]),
        &format!("Unsupported plan file version: {previous_version} (expected {current_version})"),
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
