//! End-to-end coverage for module-prefixed state lookup and the
//! version-independent `carina export` read path.
//!
//! The export fixture is a multi-file Carina directory whose state version is
//! newer than this binary supports and whose resource rows have an unknown
//! shape. Only its independently-versioned exports are readable.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const FIXTURE: &str = "tests/fixtures/state_lookup_modules_exports/carina.state.json";

fn carina(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(args)
        .output()
        .expect("failed to execute carina")
}

fn fixture_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/{}", manifest_dir, FIXTURE)
}

fn future_exports_fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("future exports fixture tempdir");
    fs::write(
        dir.path().join("backend.crn"),
        include_str!("fixtures/exports_version/backend.crn"),
    )
    .expect("write backend fixture");
    fs::write(
        dir.path().join("main.crn"),
        include_str!("fixtures/exports_version/main.crn"),
    )
    .expect("write main fixture");
    let state = serde_json::json!({
        "version": carina_state::StateFile::CURRENT_VERSION + 1,
        "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "future-exports-e2e",
        "carina_version": "future",
        "resources": [{ "future_resource_shape": { "unknown": true } }],
        "exports": {
            "regions": ["ap-northeast-1", "us-east-1"],
            "account_id": "123456789012"
        }
    });
    fs::write(
        dir.path().join("future.state.json"),
        serde_json::to_vec_pretty(&state).expect("serialize future state fixture"),
    )
    .expect("write future state fixture");
    dir
}

fn future_fixture_state_path(dir: &Path) -> String {
    dir.join("future.state.json").display().to_string()
}

#[test]
fn lookup_module_prefixed_binding_returns_raw_attribute() {
    let url = fixture_path();
    let output = carina(&["state", "lookup", "r.distribution.id", "--state-url", &url]);
    assert!(
        output.status.success(),
        "lookup failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "E2E954VKWYKT8K");
}

#[test]
fn state_lookup_exports_is_a_resource_address() {
    let url = fixture_path();
    let output = carina(&["state", "lookup", "exports", "--state-url", &url]);
    assert!(
        !output.status.success(),
        "exports must no longer be a special state lookup form.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Resource 'exports' not found in state"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn export_state_url_reads_supported_exports_from_future_state() {
    let fixture = future_exports_fixture();
    let url = future_fixture_state_path(fixture.path());
    let output = carina(&["export", "account_id", "--raw", "--state-url", &url]);
    assert!(
        output.status.success(),
        "export failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "123456789012");
    assert!(output.stderr.is_empty());
}

#[test]
fn export_state_url_json_prints_the_sorted_decoded_map() {
    let fixture = future_exports_fixture();
    let url = future_fixture_state_path(fixture.path());
    let output = carina(&["export", "--json", "--state-url", &url]);
    assert!(
        output.status.success(),
        "export failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("export --json must emit valid JSON");
    assert_eq!(
        parsed,
        serde_json::json!({
            "account_id": "123456789012",
            "regions": ["ap-northeast-1", "us-east-1"]
        })
    );
    assert!(
        stdout.find("account_id").unwrap() < stdout.find("regions").unwrap(),
        "export keys must be sorted: {stdout}"
    );
}

#[test]
fn export_state_url_warns_when_legacy_exports_are_migrated() {
    let fixture = tempfile::tempdir().expect("legacy exports fixture tempdir");
    let state_path = fixture.path().join("legacy.state.json");
    let legacy_version = carina_state::StateFile::EXPORTS_VERSION_INTRODUCED - 1;
    let state = serde_json::json!({
        "version": legacy_version,
        "serial": 1,
        "lineage": "legacy-exports-e2e",
        "carina_version": "legacy",
        "resources": [],
        "exports": { "account_id": "123456789012" }
    });
    fs::write(
        &state_path,
        serde_json::to_vec_pretty(&state).expect("serialize legacy state fixture"),
    )
    .expect("write legacy state fixture");

    let output = carina(&[
        "export",
        "account_id",
        "--raw",
        "--state-url",
        &state_path.display().to_string(),
    ]);
    assert!(
        output.status.success(),
        "export failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let migration_marker = format!("is v{legacy_version} on disk");
    assert!(
        stderr.contains(&migration_marker) && stderr.contains("will be rewritten"),
        "legacy exports read must emit the state migration warning: {stderr}"
    );
}

#[test]
fn export_project_backend_uses_the_exports_only_path() {
    let fixture = future_exports_fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .args(["export", "--json"])
        .output()
        .expect("failed to execute carina");
    assert!(
        output.status.success(),
        "export failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("export --json must emit valid JSON");
    assert_eq!(parsed["account_id"], "123456789012");
}
