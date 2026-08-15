//! End-to-end validation regressions for root-owned upstream state and exports
//! declarations inside modules.
//!
//! Module markers and the rejected declarations live in separate `.crn`
//! files so the tests exercise the directory-scoped validation boundary.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const UPSTREAM_RULE: &str = "upstream_state declarations are not allowed inside modules";
const EXPORTS_RULE: &str = "exports blocks are not allowed inside modules";

const UPSTREAM_STATE: &str = r#"let up = upstream_state { source = '../other' }
"#;

const EXPORTS_BLOCK: &str = r#"exports {
  module_value = "module"
}
"#;

struct Fixture {
    _temp: TempDir,
    caller: PathBuf,
    module: PathBuf,
}

impl Fixture {
    fn direct() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let caller = temp.path().join("caller");
        let module = temp.path().join("module");
        let other = temp.path().join("other");
        std::fs::create_dir(&caller).expect("caller directory");
        std::fs::create_dir(&module).expect("module directory");
        std::fs::create_dir(&other).expect("upstream directory");

        write(
            &caller,
            "main.crn",
            r#"let component = use { source = '../module' }

let instance = component { }
"#,
        );
        write(
            &module,
            "arguments.crn",
            r#"arguments {
  name: String = "test"
}
"#,
        );
        write(
            &other,
            "exports.crn",
            r#"exports {
  some_export = "upstream"
}
"#,
        );

        Self {
            _temp: temp,
            caller,
            module,
        }
    }

    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(&self.caller, Vec::new())
    }
}

fn write(dir: &Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("fixture file");
}

fn assert_path_prefixed_rejection(diagnostics: &[String], rule: &str) {
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.contains("../module:") && diagnostic.contains(rule) }),
        "module declaration must be rejected with its import path: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.contains("Module resolution error")),
        "the resolver backstop must not also reach validate output: {diagnostics:#?}",
    );
    assert_eq!(
        diagnostics.len(),
        1,
        "the module declaration must produce one collected validation message: {diagnostics:#?}",
    );
}

fn state_refresh_stderr(fixture: &Fixture) -> String {
    write(
        &fixture.caller,
        "backend.crn",
        r#"backend local {
  path = "carina.state.json"
}
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(&fixture.caller)
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["state", "refresh", "--lock=false", "."])
        .output()
        .expect("run carina state refresh");

    String::from_utf8(output.stderr).expect("state refresh stderr must be UTF-8")
}

fn assert_resolver_backstop(stderr: &str, rule: &str) {
    assert!(
        stderr.contains(&format!("Module resolution error: {rule}")),
        "the skip-validation path must retain the resolver backstop form; stderr: {stderr}",
    );
    assert!(
        !stderr.contains(&format!("../module: {rule}")),
        "state refresh must bypass the path-prefixed validation walk; stderr: {stderr}",
    );
}

#[test]
fn upstream_state_in_sibling_module_file_is_rejected() {
    let fixture = Fixture::direct();
    write(&fixture.module, "upstream.crn", UPSTREAM_STATE);

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, UPSTREAM_RULE);
}

#[test]
fn exports_in_sibling_module_file_are_rejected() {
    let fixture = Fixture::direct();
    write(&fixture.module, "exports.crn", EXPORTS_BLOCK);

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, EXPORTS_RULE);
}

#[test]
fn upstream_state_in_root_directory_is_allowed() {
    let fixture = Fixture::direct();
    write(&fixture.caller, "upstream.crn", UPSTREAM_STATE);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "root upstream_state must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn exports_in_root_directory_are_allowed() {
    let fixture = Fixture::direct();
    write(&fixture.caller, "exports.crn", EXPORTS_BLOCK);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "root exports must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn state_refresh_uses_upstream_state_resolver_backstop_when_validation_walk_is_skipped() {
    let fixture = Fixture::direct();
    write(&fixture.module, "upstream.crn", UPSTREAM_STATE);

    let stderr = state_refresh_stderr(&fixture);

    assert_resolver_backstop(&stderr, UPSTREAM_RULE);
}

#[test]
fn state_refresh_uses_exports_resolver_backstop_when_validation_walk_is_skipped() {
    let fixture = Fixture::direct();
    write(&fixture.module, "exports.crn", EXPORTS_BLOCK);

    let stderr = state_refresh_stderr(&fixture);

    assert_resolver_backstop(&stderr, EXPORTS_RULE);
}
