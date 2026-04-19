//! Integration tests for `carina init` validation.
//!
//! These tests verify that `init` rejects projects whose provider blocks
//! cannot be resolved (e.g. missing `source`), instead of silently claiming
//! success.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn carina_init(project_dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["init", project_dir.to_str().unwrap()])
        .output()
        .expect("failed to execute carina init")
}

#[test]
fn init_fails_when_provider_has_no_source() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    fs::write(
        project.join("main.crn"),
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.s3.bucket {
  name = 'test'
}
"#,
    )
    .unwrap();

    let output = carina_init(project);

    assert!(
        !output.status.success(),
        "expected init to fail but it succeeded.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no source configured"),
        "expected error about missing source, got stderr:\n{}",
        stderr,
    );

    assert!(
        !project.join(".carina").exists(),
        ".carina/ should not be created on failure",
    );
    assert!(
        !project.join("carina-backend.lock").exists(),
        "carina-backend.lock should not be created on failure",
    );
    assert!(
        !project.join("carina-providers.lock").exists(),
        "carina-providers.lock should not be created on failure",
    );
}
