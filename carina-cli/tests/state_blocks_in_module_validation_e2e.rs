//! End-to-end validation regressions for carina#3768.
//!
//! State blocks and module markers may live in different `.crn` files, so
//! these fixtures exercise the directory-scoped module loader used by the real
//! CLI validation pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const RULE: &str = "state blocks (moved, removed, and import) are not allowed inside modules";

const MOVED_BLOCK: &str = r#"moved {
  from = mock.test.Resource 'old'
  to = mock.test.Resource 'new'
}
"#;

const REMOVED_BLOCK: &str = r#"removed {
  from = mock.test.Resource 'old'
}
"#;

const IMPORT_BLOCK: &str = r#"import {
  to = mock.test.Resource 'existing'
  id = 'resource-123'
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
        write_module_arguments(&module);

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

fn write_module_arguments(dir: &Path) {
    write(
        dir,
        "arguments.crn",
        r#"arguments {
  name: String = "test"
}
"#,
    );
}

fn assert_path_prefixed_rejection(diagnostics: &[String], path: &str) {
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains(&format!("{path}:")) && diagnostic.contains(RULE)
        }),
        "module state block must be rejected with its import path: {diagnostics:#?}",
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
        "state-block-in-module must produce one collected validation message: {diagnostics:#?}",
    );
}

#[test]
fn moved_in_sibling_module_file_is_rejected() {
    // This is the carina#3768 repro: the parser accepts the block, but module
    // expansion used to discard it without a plan effect or diagnostic.
    let fixture = Fixture::direct();
    write(&fixture.module, "state.crn", MOVED_BLOCK);

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, "../module");
}

#[test]
fn removed_in_sibling_module_file_is_rejected() {
    let fixture = Fixture::direct();
    write(&fixture.module, "state.crn", REMOVED_BLOCK);

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, "../module");
}

#[test]
fn import_in_sibling_module_file_is_rejected() {
    let fixture = Fixture::direct();
    write(&fixture.module, "state.crn", IMPORT_BLOCK);

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, "../module");
}

#[test]
fn state_blocks_in_root_directory_are_allowed() {
    let fixture = Fixture::direct();
    write(
        &fixture.caller,
        "state.crn",
        &format!("{MOVED_BLOCK}\n{REMOVED_BLOCK}\n{IMPORT_BLOCK}"),
    );

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "root state blocks must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn module_without_state_blocks_is_allowed() {
    let fixture = Fixture::direct();

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "module without state blocks must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn state_block_in_nested_module_is_rejected() {
    let fixture = Fixture::direct();
    let nested = fixture.module.join("nested");
    std::fs::create_dir(&nested).expect("nested module directory");
    write_module_arguments(&nested);
    write(&nested, "state.crn", MOVED_BLOCK);
    write(
        &fixture.module,
        "nested_call.crn",
        r#"let nested_component = use { source = 'nested' }

let nested_instance = nested_component { }
"#,
    );

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, "../module/nested");
}

#[test]
fn state_block_in_imported_but_uncalled_nested_module_is_rejected() {
    let fixture = Fixture::direct();
    let helper = fixture.module.join("helper");
    std::fs::create_dir(&helper).expect("helper module directory");
    write_module_arguments(&helper);
    write(&helper, "state.crn", REMOVED_BLOCK);
    write(
        &fixture.module,
        "helper_import.crn",
        "let helper_component = use { source = './helper' }\n",
    );

    let diagnostics = fixture.validate();

    assert_path_prefixed_rejection(&diagnostics, "../module/./helper");
}

#[test]
fn state_refresh_uses_state_block_resolver_backstop_when_validation_walk_is_skipped() {
    let fixture = Fixture::direct();
    write(&fixture.module, "state.crn", IMPORT_BLOCK);
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

#[test]
fn plan_rejects_state_block_with_path_prefixed_validation_error() {
    let fixture = Fixture::direct();
    write(&fixture.module, "state.crn", MOVED_BLOCK);

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(&fixture.caller)
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["plan", "."])
        .output()
        .expect("run carina plan");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "plan must reject the invalid module; stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stderr.contains(&format!("../module: {RULE}")),
        "plan must retain the path-prefixed validation finding; stderr: {stderr}",
    );
    assert!(
        !stderr.contains("Module resolution error"),
        "plan must stop at the user-facing validation gate; stderr: {stderr}",
    );
}
