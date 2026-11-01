use std::process::Command;

fn run(command: &str, claims: &str) -> std::process::Output {
    let directory = tempfile::tempdir().unwrap();
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let wasm = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("build carina-provider-mock for wasm32-wasip2 before this test");
    let source = format!("file://{}", wasm.display());
    std::fs::write(
        directory.path().join("providers.crn"),
        format!("provider mock {{ source = '{source}' }}\n"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        format!("let subject = mock.actions.OidcSubject {{\n  claim_keys = [{claims}]\n}}\n"),
    )
    .unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory.path())
        .args(["init", ".", "--accept-egress", &source])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory.path())
        .args([command, "."])
        .output()
        .unwrap()
}

#[test]
fn validate_rejects_duplicate_canonical_list_elements() {
    for claims in [
        "repo, context, repo",
        "repo, mock.actions.OidcSubject.ClaimKey.repo",
    ] {
        let output = run("validate", claims);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "duplicates were accepted: {stderr}"
        );
        assert!(stderr.contains("Duplicate list element"), "{stderr}");
        assert!(stderr.contains("repo"), "{stderr}");
        assert!(stderr.contains("claim_keys"), "{stderr}");
    }
}

#[test]
fn validate_accepts_distinct_list_elements() {
    let output = run(
        "validate",
        "repo, mock.actions.OidcSubject.ClaimKey.context",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plan_rejects_duplicate_canonical_list_elements() {
    let output = run("plan", "repo, mock.actions.OidcSubject.ClaimKey.repo");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "duplicates were accepted: {stderr}"
    );
    assert!(stderr.contains("Duplicate list element"), "{stderr}");
    assert!(stderr.contains("claim_keys"), "{stderr}");
}
