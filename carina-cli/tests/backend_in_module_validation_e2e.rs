//! End-to-end validation regressions for root-owned backends in modules.
//!
//! Backend declarations and module markers may live in different `.crn`
//! files, so these fixtures exercise the directory-scoped module loader used
//! by the real CLI validation pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const RULE: &str = "backend blocks are not allowed inside modules";
const BACKEND_BLOCK: &str = r#"backend local {
  path = "module.state.json"
}
"#;

const MOVED_BLOCK: &str = r#"moved {
  from = mock.test.Resource 'old'
  to = mock.test.Resource 'new'
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
        std::fs::create_dir(&caller).expect("caller directory");
        std::fs::create_dir(&module).expect("module directory");

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

#[test]
fn backend_in_sibling_module_file_is_rejected() {
    let fixture = Fixture::direct();
    write(&fixture.module, "backend.crn", BACKEND_BLOCK);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.contains("../module:") && diagnostic.contains(RULE) }),
        "module backend must be rejected with its import path: {diagnostics:#?}",
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
        "backend-in-module must produce one collected validation message: {diagnostics:#?}",
    );
}

#[test]
fn backend_in_root_directory_is_allowed() {
    let fixture = Fixture::direct();
    write(&fixture.caller, "backend.crn", BACKEND_BLOCK);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "root backend must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn backend_in_nested_module_is_rejected() {
    let fixture = Fixture::direct();
    let nested = fixture.module.join("nested");
    std::fs::create_dir(&nested).expect("nested module directory");
    write(
        &nested,
        "arguments.crn",
        "arguments {\n  name: String = \"test\"\n}\n",
    );
    write(&nested, "backend.crn", BACKEND_BLOCK);
    write(
        &fixture.module,
        "nested_call.crn",
        r#"let nested_component = use { source = 'nested' }

let nested_instance = nested_component { }
"#,
    );

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("../module/nested:") && diagnostic.contains(RULE)
        }),
        "nested module backend must retain its recursive path: {diagnostics:#?}",
    );
    assert_eq!(diagnostics.len(), 1);
}

#[test]
fn backend_in_imported_but_uncalled_nested_module_is_rejected() {
    let fixture = Fixture::direct();
    let helper = fixture.module.join("helper");
    std::fs::create_dir(&helper).expect("helper module directory");
    write(
        &helper,
        "arguments.crn",
        "arguments {\n  name: String = \"test\"\n}\n",
    );
    write(&helper, "backend.crn", BACKEND_BLOCK);
    write(
        &fixture.module,
        "helper_import.crn",
        "let helper_component = use { source = './helper' }\n",
    );

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("../module/./helper:") && diagnostic.contains(RULE)
        }),
        "an imported-but-uncalled backend must retain its recursive path: {diagnostics:#?}",
    );
    assert_eq!(diagnostics.len(), 1);
}

#[test]
fn state_block_and_backend_in_one_module_are_both_reported() {
    let fixture = Fixture::direct();
    write(&fixture.module, "state.crn", MOVED_BLOCK);
    write(&fixture.module, "backend.crn", BACKEND_BLOCK);

    let diagnostics = fixture.validate();

    assert_eq!(
        diagnostics.len(),
        2,
        "independent module-boundary violations must not short-circuit: {diagnostics:#?}",
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .contains("state blocks (moved, removed, and import) are not allowed inside modules")),
        "the state-block finding must be retained: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains(RULE)),
        "the backend finding must be retained: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.contains("../module:")),
        "both findings must retain the module path: {diagnostics:#?}",
    );
}

#[test]
fn root_arguments_with_backend_report_only_the_arguments_rule() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(
        temp.path(),
        "arguments.crn",
        r#"arguments {
  state_path: String = "state.json"
}
"#,
    );
    write(temp.path(), "backend.crn", BACKEND_BLOCK);

    let diagnostics =
        carina_cli::commands::validate::validate_with_factories(temp.path(), Vec::new());

    assert_eq!(
        diagnostics.len(),
        1,
        "carina#2198 must remain a single root-arguments diagnostic: {diagnostics:#?}",
    );
    assert!(
        diagnostics[0].contains("arguments blocks are only valid inside module definitions"),
        "the existing root-arguments rule must win: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.contains(RULE)),
        "root validation must not apply the module-only backend rule: {diagnostics:#?}",
    );
}

#[test]
fn state_refresh_uses_backend_resolver_backstop_when_validation_walk_is_skipped() {
    let fixture = Fixture::direct();
    write(&fixture.module, "backend.crn", BACKEND_BLOCK);
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
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains(&format!("Module resolution error: {RULE}")),
        "the skip-validation path must retain the resolver backstop form; stderr: {stderr}",
    );
    assert!(
        !stderr.contains(&format!("../module: {RULE}")),
        "state refresh must bypass the path-prefixed validation walk; stderr: {stderr}",
    );
}
