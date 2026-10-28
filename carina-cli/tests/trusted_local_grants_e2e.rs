use std::process::Command;

#[test]
fn invalid_trusted_local_env_does_not_affect_fmt_without_provider_load() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project.path())
        .env("CARINA_TRUST_LOCAL_PROVIDER", "not-a-grant")
        .args(["fmt", "."])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "fmt failed before loading a provider: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("No .crn files found."),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
