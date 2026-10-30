//! End-to-end coverage for `--state-url` on `carina state {lookup,list,show}`.
//!
//! Covers carina#3336:
//! - The clap-level `conflicts_with` between `[PATH]` and `--state-url`
//!   surfaces as a usage error when both are explicitly supplied.
//! - `lookup` resolves a query directly from a bare-path state URL with
//!   no `.crn` in cwd.
//! - `list` and `show` accept `file://` URLs.

use std::fs;
use std::process::Command;

use serde_json::json;
use tempfile::TempDir;

fn carina(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(args)
        .output()
        .expect("failed to execute carina")
}

/// Write a minimal v8 state file with a single ec2.Vpc resource bound
/// to `vpc` and return its path.
fn write_minimal_state(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("carina.state.json");
    let state = json!({
        "version": 8,
        "serial": 1,
        "lineage": "test-state-url-e2e",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "my-vpc",
                "binding": "vpc",
                "provider": "awscc",
                "identifier": "vpc-deadbeef",
                "attributes": {
                    "vpc_id": "vpc-deadbeef",
                    "cidr_block": "10.0.0.0/16"
                },
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "dependency_bindings": []
            }
        ],
        "exports": {}
    });
    fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    path
}

fn write_secret_display_state(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("carina.state.json");
    let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
    let state = json!({
        "version": 14,
        "serial": 1,
        "lineage": "secret-display-e2e",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.resource",
            "identity": "display",
            "binding": "display",
            "provider": "mock",
            "identifier": "display-id",
            "attributes": {
                "stored_secret": {"$secret": digest},
                "user_secret_map": {"$$secret": "user-visible"}
            },
            "protected": false,
            "directives": {},
            "prefixes": {},
            "name_overrides": {},
            "dependency_bindings": []
        }],
        "exports": {}
    });
    fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    path
}

fn assert_command_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "command failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn state_lookup_state_url_and_path_conflict() {
    let tmp = TempDir::new().unwrap();
    let state = write_minimal_state(tmp.path());

    let output = carina(&[
        "state",
        "lookup",
        "vpc",
        "--state-url",
        state.to_str().unwrap(),
        "/some/dir",
    ]);

    assert!(
        !output.status.success(),
        "expected the conflicting-args invocation to fail.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--state-url") && stderr.contains("[PATH]"),
        "expected clap conflict error mentioning both flags, got:\n{}",
        stderr,
    );
}

#[test]
fn state_lookup_bare_path_url_returns_attribute() {
    let tmp = TempDir::new().unwrap();
    let state = write_minimal_state(tmp.path());

    let output = carina(&[
        "state",
        "lookup",
        "vpc.vpc_id",
        "--state-url",
        state.to_str().unwrap(),
    ]);

    assert!(
        output.status.success(),
        "lookup failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "vpc-deadbeef");
}

#[test]
fn state_lookup_redacts_secrets_without_colliding_with_user_secret_keys() {
    let tmp = TempDir::new().unwrap();
    let state = write_secret_display_state(tmp.path());
    let state = state.to_str().unwrap();

    let raw_secret = carina(&[
        "state",
        "lookup",
        "display.stored_secret",
        "--state-url",
        state,
    ]);
    assert_command_success(&raw_secret);
    assert_eq!(
        String::from_utf8_lossy(&raw_secret.stdout).trim(),
        "(secret)"
    );

    let json_secret = carina(&[
        "state",
        "lookup",
        "display.stored_secret",
        "--state-url",
        state,
        "--json",
    ]);
    assert_command_success(&json_secret);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&json_secret.stdout).unwrap(),
        json!({"redacted": true})
    );

    for args in [
        vec![
            "state",
            "lookup",
            "display.user_secret_map",
            "--state-url",
            state,
        ],
        vec![
            "state",
            "lookup",
            "display.user_secret_map",
            "--state-url",
            state,
            "--json",
        ],
    ] {
        let output = carina(&args);
        assert_command_success(&output);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            json!({"$secret": "user-visible"})
        );
    }

    let digest_fragment = "argon2-legacy:73616c74";
    assert!(!String::from_utf8_lossy(&raw_secret.stdout).contains(digest_fragment));
    assert!(!String::from_utf8_lossy(&json_secret.stdout).contains(digest_fragment));
}

#[test]
fn state_list_file_url_lists_resources() {
    let tmp = TempDir::new().unwrap();
    let state = write_minimal_state(tmp.path());
    let url = format!("file://{}", state.display());

    let output = carina(&["state", "list", "--state-url", &url]);

    assert!(
        output.status.success(),
        "list failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("awscc.ec2.Vpc vpc"),
        "expected list output to mention the vpc resource, got:\n{}",
        stdout,
    );
}

#[test]
fn state_show_json_file_url_emits_versioned_public_document() {
    let tmp = TempDir::new().unwrap();
    let state = write_minimal_state(tmp.path());
    let url = format!("file://{}", state.display());

    let output = carina(&["state", "show", "--state-url", &url, "--json"]);

    assert!(
        output.status.success(),
        "show failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("show --json must emit valid JSON");
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["state"], "present");
    assert_eq!(
        parsed["resources"][0]["current"]["identifier"],
        "vpc-deadbeef"
    );
    assert!(parsed.get("lineage").is_none());
}

#[test]
fn state_lookup_unsupported_scheme_errors() {
    let output = carina(&[
        "state",
        "lookup",
        "vpc",
        "--state-url",
        "https://example.com/state.json",
    ]);

    assert!(
        !output.status.success(),
        "expected unsupported-scheme invocation to fail.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unsupported URL scheme"),
        "expected unsupported-scheme error, got:\n{}",
        stderr,
    );
}
