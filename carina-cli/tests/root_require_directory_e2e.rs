//! Root requirements are rejected at the shared directory admission boundary.

use std::collections::BTreeSet;
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
        success(&directory.run(&["init", "."]));
        directory.write_root_require(condition);
        directory
    }

    fn write_root_require(&self, condition: &str) {
        fs::write(
            self.project.join("guards.crn"),
            format!("// sibling guard\nrequire {condition}, 'root guard'\n"),
        )
        .unwrap();
    }

    fn clear_root_require(&self) {
        fs::remove_file(self.project.join("guards.crn")).unwrap();
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

    fn assert_validate_diagnostic(&self, condition: &str) {
        let message = ROOT_REQUIRE_ERROR;
        let human = self.run(&["validate", "."]);
        rejected(&human, message);
        let text = output_text(&human);
        assert!(text.contains("guards.crn at line 2"), "{text}");
        assert_eq!(text.matches(message).count(), 1, "{text}");
        assert!(!text.contains("root guard"), "{text}");
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
                "start": {"line": 2, "column": 1},
                "end": {"line": 2, "column": 23 + condition.len()}
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
                assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                assert_eq!(
                    matching[0].range,
                    Range::new(
                        Position::new(1, 0),
                        Position::new(1, 22 + condition.len() as u32)
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
fn directory_read_error_chain_renders_once() {
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
fn directory_discovery_error_chain_renders_once() {
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

fn upstream_failures() -> (
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
fn module_require_error_chain_renders_once() {
    let (source_error, _) = upstream_failures();
    let carina_core::upstream_exports::UpstreamExportSourceError::Module { error, .. } =
        source_error
    else {
        panic!("expected module failure");
    };
    let finding = error.require_outside_module().unwrap();
    assert!(finding.location().file().path().ends_with("guards.crn"));
    assert_eq!(finding.location().span().start_column(), 1);
    assert_eq!(finding.location().span().end_column(), 32);
    assert_eq!(finding.detail().to_string(), ROOT_REQUIRE_ERROR);
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(error.as_ref()),
        error.to_string()
    );
}

#[test]
fn upstream_source_error_chain_renders_once() {
    let (error, _) = upstream_failures();
    let (base, module) = error.module_failure().unwrap();
    assert!(base.ends_with("upstream"));
    assert!(module.require_outside_module().is_some());
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(&error),
        error.to_string()
    );
}

#[test]
fn upstream_report_error_chain_renders_once() {
    let (_, error) = upstream_failures();
    assert!(error.module_failure().is_some());
    assert_eq!(
        carina_cli::error::format_error_with_source_chain(&error),
        error.to_string()
    );
}

fn assert_empty_template(blocks: &str) {
    let directory = Directory::new("true");
    directory.clear_root_require();
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
fn empty_arguments_validate_as_template() {
    assert_empty_template("arguments {}\n");
}
#[test]
fn empty_attributes_validate_as_template() {
    assert_empty_template("attributes {}\n");
}
#[test]
fn both_empty_blocks_validate_as_template() {
    assert_empty_template("arguments {}\nattributes {}\n");
}

#[test]
fn validate_standalone_module_directory_keeps_template_scope() {
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

fn assert_upstream_require(condition: &str) {
    let directory = Directory::new("true");
    directory.clear_root_require();
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
    assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{report}");
    let error = report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.contains(ROOT_REQUIRE_ERROR))
        })
        .unwrap_or_else(|| panic!("{report}"));
    let file = error["location"]["file"]
        .as_str()
        .expect("upstream require must retain its authored file");
    assert!(file.ends_with("upstream/guards.crn"), "{report}");
    assert_eq!(
        error["location"]["range"],
        json!({
            "start": {"line": 2, "column": 1},
            "end": {"line": 2, "column": 27 + condition.len()}
        })
    );
    let human = directory.run(&["validate", "."]);
    let text = output_text(&human);
    assert!(text.contains("upstream/guards.crn:2:1"), "{text}");
}

#[test]
fn root_require_upstream_false_keeps_location() {
    assert_upstream_require("false");
}

#[test]
fn root_require_upstream_unknown_keeps_location() {
    assert_upstream_require("missing == 1");
}

#[test]
fn root_require_upstream_true_keeps_location() {
    assert_upstream_require("true");
}

#[test]
fn root_require_upstream_function_call_reports_placement_first() {
    assert_upstream_require("custom('x') == 1");
}

#[test]
fn root_require_rejected_without_module_calls_and_with_data_sources() {
    for main in [
        "let r = mock.test.resource { name = 'root' }\n",
        "let r = read mock.test.resource { name = 'root' }\n",
    ] {
        let directory = Directory::new("false");
        fs::write(directory.project.join("main.crn"), main).unwrap();
        rejected(&directory.run(&["validate", "."]), ROOT_REQUIRE_ERROR);
        rejected(&directory.run(&["plan", "."]), ROOT_REQUIRE_ERROR);
    }
}

#[test]
fn saved_plan_secret_verification_matches_module_policy() {
    for has_secret in [false, true] {
        for root_guard in [false, true] {
            let directory = Directory::new("true");
            directory.clear_root_require();
            let comment = if has_secret {
                "comment = secret('SAVED_PLAN_SECRET_VALUE')"
            } else {
                ""
            };
            fs::write(directory.project.join("main.crn"), format!(
                "let r = mock.test.resource {{ name = 'root' {comment} }}\nlet checked = use {{ source = './checked' }}\nlet control = checked {{ n = 2 }}\n"
            )).unwrap();
            success(&directory.run(&["plan", "--out", "saved.plan", "."]));
            let message = if root_guard {
                directory.write_root_require("false");
                ROOT_REQUIRE_ERROR
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
            assert!(!output_text(&output).contains("SAVED_PLAN_SECRET_VALUE"));
        }
    }
}

#[test]
fn root_require_saved_plan_secret_failure_exposes_the_cause() {
    let directory = Directory::new("true");
    directory.clear_root_require();
    fs::write(
        directory.project.join("main.crn"),
        "let r = mock.test.resource { name = 'root' comment = secret('SAVED_PLAN_PRIVATE_VALUE') }\n",
    )
    .unwrap();
    success(&directory.run(&["plan", "--out", "saved.plan", "."]));
    let plan = directory.project.join("saved.plan");
    let saved = fs::read(&plan).unwrap();
    let state_paths = [
        directory.project.join("carina.state.json"),
        directory.home.join("mock-state.json"),
    ];
    let before = state_paths.each_ref().map(|path| fs::read(path).ok());
    directory.write_root_require("true");
    let output = directory.run(&["apply", "--auto-approve", "saved.plan"]);
    rejected(&output, ROOT_REQUIRE_ERROR);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(
        stderr,
        format!(
            "Error: Saved-plan secret verification failed because the current configuration could not be evaluated (for example, an environment variable or decryption key may be unavailable); re-run `carina plan --out saved.plan {project}` after restoring the required inputs: {project}/guards.crn at line 2\nError: {ROOT_REQUIRE_ERROR}\n",
            project = directory.project.display(),
        )
    );
    assert_eq!(stderr.matches(ROOT_REQUIRE_ERROR).count(), 1);
    assert!(!output_text(&output).contains("SAVED_PLAN_PRIVATE_VALUE"));
    assert_eq!(saved, fs::read(plan).unwrap());
    assert_eq!(
        before,
        state_paths.each_ref().map(|path| fs::read(path).ok())
    );
}

fn rejected(output: &Output, message: &str) {
    let text = output_text(output);
    assert!(!output.status.success(), "unexpected success: {text}");
    assert!(text.contains(message), "{text}");
    assert!(!text.contains("Declare it with 'let'"), "{text}");
}

const ROOT_REQUIRE_ERROR: &str = "`require` is only allowed inside a module";

fn assert_root_rejected_on_every_path(condition: &str) {
    let directory = Directory::new(condition);
    directory.assert_validate_diagnostic(condition);
    rejected(&directory.run(&["plan", "."]), ROOT_REQUIRE_ERROR);
    rejected(
        &directory.run(&["plan", "--out", "saved.plan", "."]),
        ROOT_REQUIRE_ERROR,
    );
    assert!(!directory.project.join("saved.plan").exists());
    rejected(
        &directory.run(&["apply", "--auto-approve", "."]),
        ROOT_REQUIRE_ERROR,
    );
    let paths = [
        directory.project.join("carina.state.json"),
        directory.home.join("mock-state.json"),
    ];
    assert!(paths.iter().all(|path| !path.exists()));
    directory.clear_root_require();
    success(&directory.run(&["apply", "--auto-approve", "."]));
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    directory.write_root_require(condition);
    rejected(
        &directory.run(&["apply", "--auto-approve", "."]),
        ROOT_REQUIRE_ERROR,
    );
    assert_eq!(before, paths.each_ref().map(|path| fs::read(path).unwrap()));
}

#[test]
fn root_require_true_commands() {
    assert_root_rejected_on_every_path("true");
}

#[test]
fn root_require_false_commands() {
    assert_root_rejected_on_every_path("false");
}

#[test]
fn root_require_let_commands() {
    assert_root_rejected_on_every_path("n == 2");
}

fn assert_state_command_rejects_root_require(args: &[&str]) {
    let directory = Directory::new("true");
    directory.clear_root_require();
    success(&directory.run(&["apply", "--auto-approve", "."]));
    let paths = [
        directory.project.join("carina.state.json"),
        directory.home.join("mock-state.json"),
    ];
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    directory.write_root_require("true");
    let output = directory.run(args);
    rejected(&output, ROOT_REQUIRE_ERROR);
    let text = output_text(&output);
    assert_eq!(text.matches(ROOT_REQUIRE_ERROR).count(), 1, "{text}");
    assert!(text.contains("guards.crn"), "{text}");
    assert!(text.contains("2:1") || text.contains("line 2"), "{text}");
    assert!(!text.contains("root guard"), "{text}");
    assert_eq!(before, paths.each_ref().map(|path| fs::read(path).unwrap()));
}

#[test]
fn root_require_blocks_destroy_without_state_changes() {
    assert_state_command_rejects_root_require(&["destroy", "--auto-approve", "."]);
}

#[test]
fn root_require_blocks_state_refresh_without_state_changes() {
    assert_state_command_rejects_root_require(&["state", "refresh", "."]);
}

#[test]
fn root_require_saved_plan_contract() {
    let directory = Directory::new("true");
    rejected(
        &directory.run(&["plan", "--out", "saved.plan", "."]),
        ROOT_REQUIRE_ERROR,
    );
    assert!(!directory.project.join("saved.plan").exists());
    directory.clear_root_require();
    success(&directory.run(&["plan", "--out", "saved.plan", "."]));
    let plan_path = directory.project.join("saved.plan");
    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    assert_eq!(plan["version"], 45);
    assert!(!plan.to_string().contains("root guard"));
    // A secret-free saved plan reloads backend syntax without full validation.
    directory.write_root_require("true");
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
}

#[test]
fn root_require_does_not_infer_or_evaluate_condition() {
    for condition in [
        "r.identifier != null",
        "missing == 2",
        "n[0] == 2",
        "r['PRIVATE_ROOT_REQUIRE_MAP_KEY'] == 2",
        "true || missing == 2",
        "env('PRIVATE_ROOT_REQUIRE_MAP_KEY') == 1",
        "coalesce('unused') == 1",
        "custom('unused') == 1",
        "null",
        "1",
        "length(null) == 0",
        "len(1) == 1",
        "length() == 0",
        "1.5 + 1 == 2",
        "1 / 0 == 2",
        "true && 1",
    ] {
        let directory = Directory::new(condition);
        directory.assert_validate_diagnostic(condition);
        for args in [&["plan", "."][..], &["apply", "--auto-approve", "."]] {
            let output = directory.run(args);
            rejected(&output, ROOT_REQUIRE_ERROR);
            assert!(!output_text(&output).contains("PRIVATE_ROOT_REQUIRE_MAP_KEY"));
            assert!(!output_text(&output).contains("root guard"));
        }
    }
}

#[test]
fn root_require_one_error_per_statement() {
    let directory = Directory::new("true");
    fs::write(
        directory.project.join("guards.crn"),
        "require true, 'PRIVATE_MESSAGE'\nrequire missing == 2, 'PRIVATE_MESSAGE'\nrequire 1 / 0 == 0, 'PRIVATE_MESSAGE'\n",
    ).unwrap();
    let output = directory.run(&["validate", "--json", "."]);
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 3, "{report}");
    for (index, error) in errors.iter().enumerate() {
        assert_eq!(error["message"], ROOT_REQUIRE_ERROR);
        assert_eq!(error["location"]["file"], "guards.crn");
        assert_eq!(error["location"]["range"]["start"]["line"], index + 1);
    }
    assert!(!output_text(&output).contains("PRIVATE_MESSAGE"));
}

#[test]
fn root_require_index_builtin_does_not_expose_map_key() {
    let directory = Directory::new("false");
    rejected(&directory.run(&["validate", "."]), ROOT_REQUIRE_ERROR);
    directory.write_root_require("r[env('CARINA_ROOT_REQUIRE_KEY')] != null");
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
fn root_require_module_control_is_unchanged() {
    let directory = Directory::new("false");
    rejected(&directory.run(&["validate", "."]), ROOT_REQUIRE_ERROR);
    directory.clear_root_require();
    success(&directory.run(&["validate", "."]));
    success(&directory.run(&["validate", "--json", "."]));
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

#[test]
fn root_require_attributes_only_directory_is_template() {
    let directory = Directory::new("true");
    fs::write(
        directory.project.join("checked/arguments.crn"),
        "attributes { value: Int = 1 }\n",
    )
    .unwrap();
    fs::write(
        directory.project.join("checked/guards.crn"),
        "require false, 'template guard'\n",
    )
    .unwrap();
    success(&directory.run(&["validate", "checked"]));
    success(&directory.run(&["validate", "--json", "checked"]));
}

#[test]
fn root_require_lint_matches_validate() {
    for condition in ["true", "false", "n == 2"] {
        let directory = Directory::new(condition);
        for (file, line) in [("guards.crn", 2), ("main.crn", 5)] {
            if file == "main.crn" {
                directory.clear_root_require();
                let main = directory.project.join("main.crn");
                let source = fs::read_to_string(&main).unwrap();
                fs::write(main, format!("{source}require {condition}, 'root guard'\n")).unwrap();
            }
            let output = directory.run(&["lint", "."]);
            assert_eq!(output.status.code(), Some(1));
            assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                format!(
                    "Error: Module resolution error: ./{file}:{line}:1: {ROOT_REQUIRE_ERROR}\n"
                )
            );
        }
    }
}

#[test]
fn root_require_bootstrap_commands_do_not_publish_retained_rejection() {
    let directory = Directory::new("true");
    directory.clear_root_require();
    fs::write(
        directory.project.join("main.crn"),
        "let r = mock.test.resource { name = 'root' }\nexports { value: String = 'published' }\n",
    )
    .unwrap();
    success(&directory.run(&["apply", "--auto-approve", "."]));
    fs::write(directory.project.join("guards.crn"), "require true, 'x'\n").unwrap();
    success(&directory.run(&["fmt", "."]));
    let paths = [
        directory.project.join("carina.state.json"),
        directory.home.join("mock-state.json"),
    ];
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    for (args, expected) in [
        (
            &["init", "."][..],
            "Resolving 1 provider(s)...\n0 provider(s) installed in .carina/providers/\nInitialized successfully.\n",
        ),
        (
            &["fmt", "--check", "."][..],
            "All files are properly formatted.\n",
        ),
        (&["export"][..], "value = 'published'\n"),
        (
            &["module", "info", "."][..],
            "\u{1b}[1mFile:\u{1b}[0m \u{1b}[36munknown\u{1b}[0m\n\n\u{1b}[1m=== IMPORTS ===\u{1b}[0m\n\n  \u{1b}[2m(none)\u{1b}[0m\n\n\u{1b}[1m=== CREATES ===\u{1b}[0m\n\n  \u{1b}[97mr\u{1b}[0m: \u{1b}[33mmock.test.resource\u{1b}[0m\n\n",
        ),
        (&["state", "list", "."][..], "mock.test.resource r\n"),
    ] {
        let output = directory.run(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            output_text(&output)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            expected,
            "{args:?}"
        );
        assert_eq!(String::from_utf8(output.stderr).unwrap(), "", "{args:?}");
        assert_eq!(before, paths.each_ref().map(|path| fs::read(path).unwrap()));
    }
}

#[test]
fn root_require_state_subcommands_follow_validation_paths() {
    let directory = Directory::new("true");
    directory.clear_root_require();
    fs::write(
        directory.project.join("main.crn"),
        "let r = mock.test.resource { name = 'root' }\nexports { value: String = 'published' }\n",
    )
    .unwrap();
    success(&directory.run(&["apply", "--auto-approve", "."]));
    fs::write(directory.project.join("guards.crn"), "require true, 'x'\n").unwrap();
    let state_path = directory.project.join("carina.state.json");
    let mock_path = directory.home.join("mock-state.json");
    let state_bytes = fs::read(&state_path).unwrap();
    let mock_bytes = fs::read(&mock_path).unwrap();
    let current: Value = serde_json::from_slice(&state_bytes).unwrap();
    let mut legacy = current.clone();
    legacy["version"] = json!(20);
    for row in legacy["resources"].as_array_mut().unwrap() {
        row["dependency_bindings"] = json!([]);
        row["directives"]["depends_on"] = json!([]);
    }
    let legacy_path = directory.project.join("legacy.json");
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
    fs::write(&legacy_path, &legacy_bytes).unwrap();
    assert!(
        carina_state::check_and_migrate(std::str::from_utf8(&legacy_bytes).unwrap())
            .unwrap()
            .into_state()
            .requires_legacy_dependency_migration()
    );
    let root_error = format!("Error: ./guards.crn at line 1\nError: {ROOT_REQUIRE_ERROR}\n");
    let migration_error = format!(
        "Warning: state file legacy.json is v20 on disk; migration toward v21 was loaded in memory. Legacy dependency names, if present, will be classified against the expanded configuration and the disk state will be rewritten by the next apply, destroy, or state refresh.\n{root_error}"
    );
    let cases: &[(&[&str], i32, &str, &str)] = &[
        (&["state", "list", "."], 0, "mock.test.resource r\n", ""),
        (&["state", "lookup", "r.name", "."], 0, "root\n", ""),
        (
            &["state", "show", "."],
            0,
            "# mock.test.resource (r)\n  identifier = 'root-id'\n  name = 'root'\n",
            "",
        ),
        (&["state", "refresh", "."], 1, "", &root_error),
        (
            &[
                "state",
                "migrate",
                "--check",
                ".",
                "--state-url",
                "carina.state.json",
            ],
            0,
            "State dependency migration check passed: state is already version 21.\n",
            "",
        ),
        (
            &[
                "state",
                "migrate",
                "--check",
                ".",
                "--state-url",
                "legacy.json",
            ],
            1,
            "",
            &migration_error,
        ),
        (
            &[
                "state",
                "forget",
                "mock.test.resource",
                "r",
                "--auto-approve",
                ".",
            ],
            1,
            "Acquiring state lock...\n  ✓ Lock acquired\n  ✓ Lock released\n",
            "Error: no state row at this address is a create whose outcome is unknown; `carina state forget mock.test.resource r` only accepts create-outcome-unknown rows; use `removed { from = mock.test.resource 'r' }` for ordinary state removal\n",
        ),
        (
            &[
                "state",
                "bucket-delete",
                "root-require-state",
                "--force",
                ".",
            ],
            1,
            "",
            "Error: The configured backend does not manage an S3 bucket\n",
        ),
    ];
    let command =
        <carina_cli::commands::state::StateCommands as clap::Subcommand>::augment_subcommands(
            clap::Command::new("state"),
        );
    let declared: BTreeSet<_> = command
        .get_subcommands()
        .map(|command| command.get_name())
        .collect();
    let covered: BTreeSet<_> = cases.iter().map(|(args, ..)| args[1]).collect();
    assert_eq!(
        declared, covered,
        "every declared state subcommand needs a measured outcome"
    );
    for (args, exit, stdout, stderr) in cases {
        let output = directory.run(args);
        assert_eq!(
            output.status.code(),
            Some(*exit),
            "{args:?}: {}",
            output_text(&output)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            *stdout,
            "{args:?}"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            *stderr,
            "{args:?}"
        );
        assert_eq!(fs::read(&state_path).unwrap(), state_bytes);
        assert_eq!(fs::read(&mock_path).unwrap(), mock_bytes);
        assert_eq!(fs::read(&legacy_path).unwrap(), legacy_bytes);
    }

    // These inert settings exercise selection and validation only. The sole
    // configured provider is the builtin mock. The placement error precedes
    // bucket-provider construction, confirmation and any backend operation.
    fs::write(directory.project.join("providers.crn"), "backend s3 { bucket = 'root-require-state' key = 'state.json' region = 'ap-northeast-1' }\nprovider mock {}\n").unwrap();
    for (bucket, expected) in [
        ("root-require-state", root_error.as_str()),
        (
            "different-state",
            "Error: Bucket name 'different-state' does not match backend configuration bucket 'root-require-state'.\n",
        ),
    ] {
        let output = directory.run(&["state", "bucket-delete", bucket, "--force", "."]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
        assert_eq!(fs::read(&state_path).unwrap(), state_bytes);
        assert_eq!(fs::read(&mock_path).unwrap(), mock_bytes);
    }
    fs::write(
        directory.project.join("providers.crn"),
        "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
    )
    .unwrap();
    let mut unknown = current.clone();
    unknown["resources"].as_array_mut().unwrap().push(json!({
        "provider": "mock", "resource_type": "test.resource", "identity": "pending", "binding": "pending",
        "current": { "kind": "create_outcome_unknown", "started_at": "2026-10-30T03:00:00Z" }
    }));
    fs::write(&state_path, serde_json::to_vec_pretty(&unknown).unwrap()).unwrap();
    let output = directory.run(&[
        "state",
        "forget",
        "mock.test.resource",
        "pending",
        "--auto-approve",
        ".",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", output_text(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Acquiring state lock...\n  ✓ Lock acquired\nCreate-outcome-unknown row: mock.test.resource 'pending'\nThis row has no provider identifier and cannot be destroyed safely.\nForgot create-outcome-unknown current instance: mock.test.resource 'pending'\n  ✓ Lock released\n"
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
    let mut forgotten: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(forgotten["serial"], current["serial"].as_u64().unwrap() + 1);
    forgotten["serial"] = current["serial"].clone();
    assert_eq!(forgotten, current);
    assert_eq!(fs::read(&mock_path).unwrap(), mock_bytes);
    assert_eq!(fs::read(&legacy_path).unwrap(), legacy_bytes);
}

#[test]
fn root_require_lint_keeps_module_template_scope() {
    let directory = Directory::new("true");
    for blocks in [
        "arguments { n: Int }",
        "arguments {}",
        "attributes {}",
        "attributes { value: Int = 1 }",
    ] {
        fs::write(directory.project.join("checked/arguments.crn"), blocks).unwrap();
        fs::write(
            directory.project.join("checked/guards.crn"),
            "require false, 'template guard'\n",
        )
        .unwrap();
        success(&directory.run(&["lint", "checked"]));
    }
}

#[test]
fn root_require_blockless_import_keeps_module_context() {
    let directory = Directory::new("true");
    directory.clear_root_require();
    fs::remove_file(directory.project.join("checked/arguments.crn")).unwrap();
    fs::write(
        directory.project.join("checked/guards.crn"),
        "require true, 'blockless guard'\n",
    )
    .unwrap();
    fs::write(
        directory.project.join("main.crn"),
        "let checked = use { source = './checked' }\n",
    )
    .unwrap();
    // An imported directory is analyzed in module context. Existing callability
    // rules for a directory with no declared inputs/outputs are unchanged.
    for args in [&["validate", "."][..], &["validate", "--json", "."]] {
        let imported = directory.run(args);
        rejected(&imported, "Module not found: ./checked");
        assert!(!output_text(&imported).contains(ROOT_REQUIRE_ERROR));
    }
    fs::write(
        directory.project.join("main.crn"),
        "let checked = use { source = './checked' }\nlet call = checked {}\n",
    )
    .unwrap();
    let called = directory.run(&["validate", "."]);
    rejected(&called, "Module not found: ./checked");
    assert!(!output_text(&called).contains(ROOT_REQUIRE_ERROR));
    rejected(&directory.run(&["validate", "checked"]), ROOT_REQUIRE_ERROR);
    rejected(&directory.run(&["lint", "checked"]), ROOT_REQUIRE_ERROR);
}

#[test]
fn root_require_parse_time_errors_precede_placement() {
    for (condition, message, column) in [
        (
            "[1] == [1]",
            "Syntax error: expected validate_primary",
            Some(9),
        ),
        (
            "{ key = 1 } == null",
            "Syntax error: expected EOI or statement",
            Some(21),
        ),
        (
            "'abc'[0] == 'a'",
            "Syntax error: expected compare_op, add_op, or multiplicative_op",
            Some(14),
        ),
        (
            "r[env('K')] != null",
            "index access key must be an integer or string, got function call",
            None,
        ),
    ] {
        let directory = Directory::new(condition);
        let human = directory.run(&["validate", "."]);
        let human_message = message.strip_prefix("Syntax error: ").unwrap_or(message);
        rejected(&human, human_message);
        assert_eq!(output_text(&human).matches(human_message).count(), 1);
        assert!(!output_text(&human).contains(ROOT_REQUIRE_ERROR));
        let output = directory.run(&["validate", "--json", "."]);
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        assert_eq!(errors[0]["message"], message);
        assert_eq!(errors[0]["location"]["file"], "guards.crn");
        if let Some(column) = column {
            assert_eq!(
                errors[0]["location"]["range"],
                json!({
                    "start": {"line": 2, "column": column},
                    "end": {"line": 2, "column": column}
                })
            );
        } else {
            assert_eq!(errors[0]["location"]["range"], Value::Null);
        }
    }
}
