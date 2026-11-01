use std::process::Command;

fn run(command: &str, claims: &str) -> std::process::Output {
    let directory = project(claims, "");
    run_in(directory.path(), command)
}

fn project(claims: &str, declarations: &str) -> tempfile::TempDir {
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
        format!("{declarations}\nlet subject = mock.actions.OidcSubject {{\n  claim_keys = [{claims}]\n}}\n"),
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
    directory
}

fn run_in(directory: &std::path::Path, command: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory)
        .env("NO_COLOR", "1")
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

#[test]
fn plan_rejects_list_duplicate_from_upstream_coalesce_after_validate_passes() {
    let directory = project(
        "repo, upstream.claim ?? mock.actions.OidcSubject.ClaimKey.repo",
        "let upstream = upstream_state { source = './upstream' }",
    );
    let upstream = directory.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::copy(
        directory.path().join("providers.crn"),
        upstream.join("providers.crn"),
    )
    .unwrap();
    std::fs::write(
        upstream.join("main.crn"),
        r#"
backend local { path = "carina.state.json" }
exports {
  claim: mock.actions.OidcSubject.ClaimKey = mock.actions.OidcSubject.ClaimKey.repo
}
"#,
    )
    .unwrap();
    // The export was added to the upstream source after its last apply. Only
    // plan reads the saved state and selects the enum-valued fallback.
    let state = carina_state::StateFile::new();
    std::fs::write(
        upstream.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    let init = run_in(&upstream, "init");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let validate = run_in(directory.path(), "validate");
    assert!(
        validate.status.success(),
        "validate must accept the still-deferred element: {}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let plan = run_in(directory.path(), "plan");
    let stderr = String::from_utf8_lossy(&plan.stderr);
    assert!(
        !plan.status.success(),
        "plan accepted the resolved duplicate: {stderr}"
    );
    assert!(stderr.contains("Duplicate list element"), "{stderr}");
    assert!(stderr.contains("claim_keys"), "{stderr}");
    assert!(stderr.contains("index 1 (first at index 0)"), "{stderr}");
}
