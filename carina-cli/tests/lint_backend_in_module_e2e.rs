use std::process::Command;

const RULE: &str = "backend blocks are not allowed inside modules";

#[test]
fn lint_collects_path_prefixed_backend_in_module_once() {
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
    std::fs::write(
        module.join("backend.crn"),
        "backend local {\n  path = \"module.state.json\"\n}\n",
    )
    .expect("module backend");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", caller.to_str().expect("UTF-8 caller path")])
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
        .output()
        .expect("run carina lint");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "backend-in-module must make lint fail; stdout: {stdout}\nstderr: {stderr}",
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
        "lint must report the backend rule once; stderr: {stderr}",
    );
    assert!(
        stderr.contains("Found 1 lint warning(s)."),
        "lint must use its collected-warning summary; stderr: {stderr}",
    );
}
