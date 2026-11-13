use std::path::Path;
use std::process::Command;

const SHARDS: usize = 4;

fn check_shard(index: usize) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script = repo.join("scripts/validate-parity.py");
    // Keep raw captures for investigating failures in archived/partitioned runs.
    let evidence = tempfile::Builder::new()
        .prefix(&format!("validate-parity-shard-{index}-"))
        .tempdir()
        .unwrap()
        .keep();
    let command = || {
        let mut command = Command::new("python3");
        command
            .arg(&script)
            .args([
                "--binary",
                env!("CARGO_BIN_EXE_carina"),
                "--repeat",
                "8",
                "--repo",
            ])
            .arg(repo)
            .arg("--evidence")
            .arg(&evidence);
        command
    };
    // Python owns enumeration and validates the complete manifest and shard coverage.
    let listed = command().arg("--list-only").output().unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let output = command()
        .arg("--directory-manifest")
        .arg(evidence.join("directories.json"))
        .arg("--shard")
        .arg(format!("{index}/{SHARDS}"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "diagnostic order shard {index}/{SHARDS}; evidence: {}\nstdout:\n{}\nstderr:\n{}",
        evidence.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // A stable process-setup failure must not masquerade as a validation sweep.
    let first: serde_json::Value = serde_json::from_slice(
        &std::fs::read(evidence.join("0000-json-repeat-01.stdout")).unwrap(),
    )
    .expect("validate --json must reach the structured validation result");
    assert!(
        first["resources"].is_array() && first["status"].is_string(),
        "{first}"
    );
    eprintln!(
        "shard {index}/{SHARDS}; evidence: {}; {}",
        evidence.display(),
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap_or_default(),
    );
}

#[test]
fn validate_diagnostic_order_shard_0() {
    check_shard(0);
}

#[test]
fn validate_diagnostic_order_shard_1() {
    check_shard(1);
}

#[test]
fn validate_diagnostic_order_shard_2() {
    check_shard(2);
}

#[test]
fn validate_diagnostic_order_shard_3() {
    check_shard(3);
}
