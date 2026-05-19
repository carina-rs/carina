//! End-to-end test for issue #3111.
//!
//! `carina plan` takes no state lock (a lock on a read-only operation
//! would be overkill and serialize concurrent plans). It instead
//! detects TOCTOU drift: it fingerprints state at T0 (right after the
//! initial read) and re-reads just before display; if a concurrent
//! `apply`/`destroy` wrote state in that window, `plan` prints a
//! warning so the user knows the displayed plan may be stale.
//!
//! The unit tests in `commands::plan::state_drift_tests` pin the pure
//! classification (`detect_state_drift`). They do **not** prove the
//! real `run_plan` path actually re-reads state and emits the warning
//! — a green unit test calling the helper directly is not evidence the
//! apply/runtime path runs it. This file drives the real `carina plan`
//! binary with a deterministic concurrent writer slipped into the
//! T0..T1 window via the `CARINA_TEST_PLAN_DRIFT_HANDSHAKE_DIR` seam.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

fn plain_text_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    cmd
}

/// Write a minimal local-backend project plus a hand-authored state
/// file so state already exists at T0 (drift detection is moot when no
/// state exists yet — there is nothing a concurrent writer could
/// stale). Returns the path to `carina.state.json`.
fn init_project_with_state(
    dir: &std::path::Path,
    serial: u64,
    lineage: &str,
) -> std::path::PathBuf {
    fs::write(
        dir.join("main.crn"),
        r#"backend local { path = "carina.state.json" }
exports { region: String = "ap-northeast-1" }"#,
    )
    .unwrap();

    let state = serde_json::json!({
        "version": 6,
        "serial": serial,
        "lineage": lineage,
        "carina_version": "0.0.0-test",
        "resources": [],
        "exports": { "region": "ap-northeast-1" }
    });
    let state_path = dir.join("carina.state.json");
    fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    let status = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("failed to execute carina init");
    assert!(status.success(), "carina init failed");

    state_path
}

/// Spawn `carina plan` and run `concurrent_writer` *deterministically*
/// between the binary's T0 state read and its T1 re-read, using the
/// binary's file-handshake seam (a fixed sleep would race the binary's
/// own startup cost and flake). The binary creates `t0_done` once it
/// has captured the T0 fingerprint and then blocks until this function
/// creates `proceed`; the concurrent write happens strictly in that
/// window. Pass a no-op closure for the control (no-drift) case.
/// Returns the child's stderr.
fn plan_with_concurrent_writer(dir: &std::path::Path, concurrent_writer: impl FnOnce()) -> String {
    let handshake = TempDir::new().unwrap();
    let t0_done = handshake.path().join("t0_done");
    let proceed = handshake.path().join("proceed");

    // Run from inside the project dir, exactly as a real user would
    // (`cd project && carina plan .`). The `backend local { path =
    // "carina.state.json" }` path is relative and resolves against the
    // process CWD, not the plan-path arg — without this the binary
    // would read an unrelated state file and never observe the drift.
    let child = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .args(["plan", "."])
        .env(
            "CARINA_TEST_PLAN_DRIFT_HANDSHAKE_DIR",
            handshake.path().to_str().unwrap(),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn carina plan");

    // Wait for the binary to signal it has captured the T0 snapshot.
    let start = std::time::Instant::now();
    while !t0_done.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "carina plan never reached the T0 handshake"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // T0 is captured; mutate state now — strictly inside the window.
    concurrent_writer();

    // Release the binary into its T1 re-read.
    fs::write(&proceed, b"").unwrap();

    let output = child.wait_with_output().expect("wait for carina plan");

    assert!(
        output.status.success(),
        "carina plan should still succeed (drift is a warning, not an error).\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stderr).expect("stderr is not UTF-8")
}

#[test]
fn concurrent_apply_between_read_and_display_emits_drift_warning() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let state_path = init_project_with_state(dir, 7, "lineage-A");

    let writer_path = state_path.clone();
    let stderr = plan_with_concurrent_writer(dir, move || {
        // Simulate a concurrent `apply`: same lineage, bumped serial.
        let state = serde_json::json!({
            "version": 6,
            "serial": 8,
            "lineage": "lineage-A",
            "carina_version": "0.0.0-test",
            "resources": [],
            "exports": { "region": "ap-northeast-1" }
        });
        fs::write(&writer_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
    });

    assert!(
        stderr.contains("state changed during plan") && stderr.contains("serial advanced 7 -> 8"),
        "expected a serial-drift warning on stderr, got:\n{stderr}"
    );
}

#[test]
fn no_concurrent_writer_emits_no_drift_warning() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project_with_state(dir, 7, "lineage-A");

    // Control: nothing writes state during the window.
    let stderr = plan_with_concurrent_writer(dir, || {});

    assert!(
        !stderr.contains("state changed during plan"),
        "no concurrent writer => no drift warning, got:\n{stderr}"
    );
}

#[test]
fn unreadable_state_at_t1_warns_it_could_not_be_rechecked() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let state_path = init_project_with_state(dir, 7, "lineage-A");

    let writer_path = state_path.clone();
    let stderr = plan_with_concurrent_writer(dir, move || {
        // Concurrent writer corrupts state mid-write: the T1 re-read
        // fails to deserialize, exercising the read-error branch
        // (which must warn, not abort — the plan already computed
        // against the intact T0 snapshot).
        fs::write(&writer_path, b"{ this is not valid json").unwrap();
    });

    assert!(
        stderr.contains("could not re-read state to check for concurrent changes"),
        "expected a re-read-failure warning on stderr, got:\n{stderr}"
    );
}

#[test]
fn no_state_at_t0_skips_drift_check_entirely() {
    // First-ever plan: no state file exists. There is no T0 baseline,
    // so the whole re-read/warn block must be skipped — neither the
    // drift warning nor the "could not re-read state" warning may
    // appear (the latter was a spurious-warning regression risk on
    // first-run / bootstrap plans with a transient backend error).
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    fs::write(
        dir.join("main.crn"),
        r#"backend local { path = "carina.state.json" }
exports { region: String = "ap-northeast-1" }"#,
    )
    .unwrap();
    let status = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("failed to execute carina init");
    assert!(status.success(), "carina init failed");

    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .args(["plan", "."])
        .output()
        .expect("failed to execute carina plan");
    assert!(
        output.status.success(),
        "carina plan failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is not UTF-8");

    assert!(
        !stderr.contains("state changed during plan")
            && !stderr.contains("could not re-read state"),
        "no state at T0 => no drift / re-read warning of any kind, got:\n{stderr}"
    );
}
