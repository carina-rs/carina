//! End-to-end validation regressions for carina#3717.
//!
//! Provider declarations and module markers may live in different `.crn`
//! files, so these fixtures exercise the directory-scoped module loader used
//! by the real CLI validation pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

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

fn write_provider(dir: &Path) {
    write(dir, "providers.crn", "provider mock { }\n");
}

#[test]
fn provider_in_sibling_module_file_is_rejected() {
    // Before the validation-walk gate, the resolver rejected this only during
    // expansion as an unqualified `Module resolution error`. The early gate
    // must replace that user-facing form rather than duplicate it.
    let fixture = Fixture::direct();
    write_provider(&fixture.module);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("../module:")
                && diagnostic.contains("provider blocks are not allowed inside modules")
        }),
        "module provider must be rejected with its import path: {diagnostics:#?}",
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
        "provider-in-module must produce one collected validation message: {diagnostics:#?}",
    );
}

#[test]
fn provider_in_root_directory_is_allowed() {
    let fixture = Fixture::direct();
    write_provider(&fixture.caller);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "root provider must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn module_without_provider_is_allowed() {
    let fixture = Fixture::direct();

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.is_empty(),
        "module without a provider must remain valid: {diagnostics:#?}",
    );
}

#[test]
fn provider_in_nested_module_is_rejected() {
    let fixture = Fixture::direct();
    let nested = fixture.module.join("nested");
    std::fs::create_dir(&nested).expect("nested module directory");
    write_module_arguments(&nested);
    write_provider(&nested);
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
            diagnostic.contains("../module/nested:")
                && diagnostic.contains("provider blocks are not allowed inside modules")
        }),
        "nested module provider must be rejected with its recursive import path: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.contains("Module resolution error")),
        "the nested resolver backstop must not also reach validate output: {diagnostics:#?}",
    );
    assert_eq!(
        diagnostics.len(),
        1,
        "nested provider-in-module must produce one collected validation message: {diagnostics:#?}",
    );
}

#[test]
fn provider_in_imported_but_uncalled_nested_module_is_rejected() {
    let fixture = Fixture::direct();
    let helper = fixture.module.join("helper");
    std::fs::create_dir(&helper).expect("helper module directory");
    write_module_arguments(&helper);
    write_provider(&helper);
    write(
        &fixture.module,
        "helper_import.crn",
        "let helper_component = use { source = './helper' }\n",
    );

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("../module/./helper:")
                && diagnostic.contains("provider blocks are not allowed inside modules")
        }),
        "an imported-but-uncalled helper is still part of the module directory unit: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.contains("Module resolution error")),
        "the recursive validation walk must report before expansion: {diagnostics:#?}",
    );
    assert_eq!(
        diagnostics.len(),
        1,
        "the uncalled helper must produce one collected validation message: {diagnostics:#?}",
    );
}

#[test]
fn state_refresh_uses_resolver_backstop_when_validation_walk_is_skipped() {
    let fixture = Fixture::direct();
    write_provider(&fixture.module);
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "state refresh must reject the invalid module; stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stderr.contains("Module resolution error: provider blocks are not allowed inside modules"),
        "the skip-validation path must retain the resolver backstop form; stderr: {stderr}",
    );
    assert!(
        !stderr.contains("../module: provider blocks are not allowed inside modules"),
        "state refresh must bypass the path-prefixed validation walk; stderr: {stderr}",
    );
}
