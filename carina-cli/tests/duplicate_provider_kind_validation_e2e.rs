use std::process::Command;

fn duplicate_sourced_provider_fixture() -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    for file in ["main.crn", "providers.crn"] {
        std::fs::write(
            fixture.path().join(file),
            "provider mock {\n  source = 'file:///definitely-missing/mock.wasm'\n}\n",
        )
        .expect("fixture file");
    }
    fixture
}

fn run(command: &str, fixture: &tempfile::TempDir) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args([
            command,
            fixture.path().to_str().expect("UTF-8 fixture path"),
        ])
        .output()
        .unwrap_or_else(|error| panic!("run carina {command}: {error}"))
}

#[test]
fn validate_collects_shared_and_loader_duplicate_provider_kind_errors() {
    let fixture = duplicate_sourced_provider_fixture();

    let output = run("validate", &fixture);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "duplicate provider kinds must fail validation; stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stderr.contains("duplicate provider kind 'mock'"),
        "shared core diagnostic must be retained: {stderr}",
    );
    assert!(
        stderr.contains("declared more than once with a source"),
        "loader invariant must be retained alongside collected errors: {stderr}",
    );
}

#[test]
fn lint_collects_shared_and_loader_duplicate_provider_kind_errors() {
    let fixture = duplicate_sourced_provider_fixture();

    let output = run("lint", &fixture);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "duplicate provider kinds must fail lint; stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stderr.contains("duplicate provider kind 'mock'"),
        "shared core diagnostic must be retained: {stderr}",
    );
    assert!(
        stderr.contains("declared more than once with a source"),
        "loader invariant must be retained alongside lint findings: {stderr}",
    );
}
