//! End-to-end coverage for `carina state lookup` against module-
//! prefixed bindings and `state.exports` keys (carina#3338).
//!
//! The bulk of the resolution logic is covered by unit tests in
//! `commands::state::tests`; this file pins that the real binary,
//! invoked through `--state-url` against a fixture, returns the
//! expected raw value with no stderr noise (so the output is safe to
//! pipe into a script).

use std::process::Command;

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
fn lookup_exports_key_returns_raw_value() {
    let url = fixture_path();
    let output = carina(&[
        "state",
        "lookup",
        "exports.cloudfront_distribution_id",
        "--state-url",
        &url,
    ]);
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
fn lookup_exports_whole_map_returns_json_object() {
    let url = fixture_path();
    let output = carina(&["state", "lookup", "exports", "--state-url", &url]);
    assert!(
        output.status.success(),
        "lookup failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("lookup exports must emit valid JSON");
    assert_eq!(parsed["cloudfront_distribution_id"], "E2E954VKWYKT8K");
    assert_eq!(parsed["zone_id"], "Z008131930MO3U3NYWJTM");
}

#[test]
fn lookup_missing_export_key_fails_with_clear_message() {
    let url = fixture_path();
    let output = carina(&[
        "state",
        "lookup",
        "exports.does_not_exist",
        "--state-url",
        &url,
    ]);
    assert!(
        !output.status.success(),
        "expected lookup of missing export to fail.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does_not_exist"),
        "expected error to name the missing key; got:\n{}",
        stderr,
    );
}
