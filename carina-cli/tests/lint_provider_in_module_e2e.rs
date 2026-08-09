use std::path::Path;
use std::process::{Command, Output};

const RULE: &str = "provider blocks are not allowed inside modules";
const BLOCK_SYNTAX_RULE: &str = "Prefer block syntax for 'rules'";

fn run_lint(caller: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", caller.to_str().expect("UTF-8 caller path")])
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
        .output()
        .expect("run carina lint")
}

#[test]
fn lint_collects_path_prefixed_provider_in_module_once() {
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
    std::fs::write(module.join("providers.crn"), "provider mock { }\n").expect("module provider");

    let output = run_lint(&caller);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "provider-in-module must make lint fail; stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stderr.contains(&format!("../module: {RULE}")),
        "lint must report the path-prefixed validation finding; stderr: {stderr}",
    );
    assert!(
        !stderr.contains("Module resolution error"),
        "lint must not abort through the resolver backstop; stderr: {stderr}",
    );
    assert_eq!(
        stderr.matches(RULE).count(),
        1,
        "lint must report the provider rule once; stderr: {stderr}",
    );
    assert!(
        stderr.contains("Found 1 lint warning(s)."),
        "lint must use its collected-warning summary; stderr: {stderr}",
    );
}

#[test]
fn provider_finding_deliberately_suspends_expansion_dependent_block_syntax_checks() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let caller = fixture.path().join("caller");
    let module = fixture.path().join("module");
    std::fs::create_dir(&caller).expect("caller directory");
    std::fs::create_dir(&module).expect("module directory");
    std::fs::write(
        caller.join("main.crn"),
        r#"let component = use { source = '../module' }

let instance = component {
  rules = [{ action = "allow" }]
}
"#,
    )
    .expect("caller fixture");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  rules: list(struct { action: String })
}

let target = mock.test.resource {
  name = "target"
  rules = rules
}
"#,
    )
    .expect("module fixture");

    // First prove that expansion supplies the resource schema which makes the
    // root module-call literal eligible for the List<Struct> suggestion.
    let clean_output = run_lint(&caller);
    let clean_stderr = String::from_utf8_lossy(&clean_output.stderr);
    assert!(
        clean_stderr.contains(BLOCK_SYNTAX_RULE),
        "the clean control must report the schema-dependent warning; stderr: {clean_stderr}",
    );

    std::fs::write(module.join("providers.crn"), "provider mock { }\n").expect("module provider");

    let broken_output = run_lint(&caller);
    let broken_stdout = String::from_utf8_lossy(&broken_output.stdout);
    let broken_stderr = String::from_utf8_lossy(&broken_output.stderr);

    assert!(
        !broken_output.status.success(),
        "provider-in-module must make lint fail; stdout: {broken_stdout}\nstderr: {broken_stderr}",
    );
    assert!(
        broken_stderr.contains(&format!("../module: {RULE}")),
        "lint must retain the blocking provider finding; stderr: {broken_stderr}",
    );
    // This absence is the documented degraded mode, not a bug: expansion is
    // suspended for the broken module, so its resource cannot seed the schema-
    // dependent List<Struct> scan until the provider finding is fixed.
    assert!(
        !broken_stderr.contains(BLOCK_SYNTAX_RULE),
        "schema-dependent warnings must remain suspended; stderr: {broken_stderr}",
    );
    assert!(
        broken_stderr.contains("Found 1 lint warning(s)."),
        "lint must finish its scans and report exactly the provider finding; stderr: {broken_stderr}",
    );
}
