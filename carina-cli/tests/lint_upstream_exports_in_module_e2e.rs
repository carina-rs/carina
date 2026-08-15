use std::path::Path;
use std::process::{Command, Output};

const UPSTREAM_RULE: &str = "upstream_state declarations are not allowed inside modules";
const EXPORTS_RULE: &str = "exports blocks are not allowed inside modules";

fn run_lint(caller: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", caller.to_str().expect("UTF-8 caller path")])
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
        .output()
        .expect("run carina lint")
}

fn fixture_with_module_file(name: &str, source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let fixture = tempfile::tempdir().expect("tempdir");
    let caller = fixture.path().join("caller");
    let module = fixture.path().join("module");
    std::fs::create_dir(&caller).expect("caller directory");
    std::fs::create_dir(&module).expect("module directory");
    std::fs::write(
        caller.join("main.crn"),
        "let component = use { source = '../module' }\n\nlet instance = component { }\n",
    )
    .expect("caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  name: String = \"test\"\n}\n",
    )
    .expect("module arguments");
    std::fs::write(module.join(name), source).expect("module declaration");
    (fixture, caller)
}

fn assert_path_prefixed_lint_finding(output: &Output, rule: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "invalid module must make lint fail; stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stderr.contains(&format!("../module: {rule}")),
        "lint must report the path-prefixed validation finding; stderr: {stderr}",
    );
    assert!(
        !stderr.contains("Module resolution error"),
        "lint must not abort through the resolver backstop; stderr: {stderr}",
    );
    assert_eq!(
        stderr.matches(rule).count(),
        1,
        "lint must report the rule once; stderr: {stderr}",
    );
    assert!(
        stderr.contains("Found 1 lint warning(s)."),
        "lint must use its collected-warning summary; stderr: {stderr}",
    );
}

#[test]
fn lint_collects_path_prefixed_upstream_state_in_module_once() {
    let (_fixture, caller) = fixture_with_module_file(
        "upstream.crn",
        "let up = upstream_state { source = '../other' }\n",
    );

    let output = run_lint(&caller);

    assert_path_prefixed_lint_finding(&output, UPSTREAM_RULE);
}

#[test]
fn lint_collects_path_prefixed_exports_in_module_once() {
    let (_fixture, caller) =
        fixture_with_module_file("exports.crn", "exports {\n  module_value = \"module\"\n}\n");

    let output = run_lint(&caller);

    assert_path_prefixed_lint_finding(&output, EXPORTS_RULE);
}
