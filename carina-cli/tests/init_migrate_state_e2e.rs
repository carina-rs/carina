//! End-to-end tests for `carina init --migrate-state` (issue #3160).
//!
//! These drive the real `carina` binary against a multi-file project
//! directory (mirroring the directory-scoped infra shape: `main.crn` +
//! `backend.crn`), exercising the directory-refactor scenario from the
//! issue: a backend address change must be a hard error without the
//! flag, and must move the state file with it.
//!
//! Local → local is used so the test needs no AWS credentials while
//! still exercising the real lock-drift detection + migration path
//! through the binary.

use std::fs;
use std::process::Command;

use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

/// Build a real `StateFile` JSON with `n` resources under `lineage`,
/// serialized via serde so the shape always matches the current state
/// schema (hand-written JSON drifts and misses fields like `kind`).
fn state_json(lineage: &str, n: usize) -> String {
    let mut s = StateFile::new();
    s.serial = 3;
    s.lineage = lineage.to_string();
    for i in 0..n {
        s.resources.push(
            ResourceState::new("s3.Bucket", format!("demo{i}"), "aws")
                .with_identifier(format!("demo-bucket-{i}")),
        );
    }
    serde_json::to_string_pretty(&s).expect("serialize state fixture")
}

fn carina(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(args)
        .output()
        .expect("failed to execute carina")
}

/// Write the two-file project: a no-provider config plus a backend file
/// pointing at `state_file` (relative to the project dir).
fn write_project(project: &std::path::Path, state_file: &str) {
    fs::write(
        project.join("main.crn"),
        // No providers ⇒ init does not need to download plugins.
        "exports {\n}\n",
    )
    .unwrap();
    fs::write(
        project.join("backend.crn"),
        format!("backend local {{ path = \"{state_file}\" }}\n"),
    )
    .unwrap();
}

#[test]
fn init_then_backend_change_blocks_without_flag_and_migrates_with_it() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    let project_str = project.to_str().unwrap();

    // 1. Initialize against the original backend address.
    write_project(project, "old.state.json");
    let out = carina(&["init", project_str]);
    assert!(
        out.status.success(),
        "first init should succeed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        project.join("carina-backend.lock").exists(),
        "init should create the backend lock",
    );

    // Simulate real managed state at the old address (non-empty so the
    // migration has something meaningful to move and verify).
    fs::write(project.join("old.state.json"), state_json("lineage-abc", 1)).unwrap();

    // 2. Refactor: the backend address changes (the issue's core case).
    write_project(project, "new.state.json");

    // Bare `carina init` must REFUSE and point at --migrate-state.
    let out = carina(&["init", project_str]);
    assert!(
        !out.status.success(),
        "init must fail on backend drift without --migrate-state",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Backend configuration changed") && stderr.contains("--migrate-state"),
        "drift error must name --migrate-state, got:\n{stderr}",
    );
    assert!(
        !project.join("new.state.json").exists(),
        "a refused init must not create the new state file",
    );
    assert!(
        project.join("old.state.json").exists(),
        "a refused init must not touch the old state file",
    );

    // 3. `--migrate-state` moves the state and re-locks.
    let out = carina(&["init", "--migrate-state", project_str]);
    assert!(
        out.status.success(),
        "init --migrate-state should succeed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let new_state = project.join("new.state.json");
    assert!(new_state.exists(), "state must be migrated to the new path");
    let migrated = fs::read_to_string(&new_state).unwrap();
    assert!(
        migrated.contains("lineage-abc") && migrated.contains("demo-bucket"),
        "migrated state must preserve lineage and resources, got:\n{migrated}",
    );
    // local → remote-style cleanup: source local file removed.
    assert!(
        !project.join("old.state.json").exists(),
        "local source state should be deleted after a verified copy",
    );

    // 4. The lock now matches ⇒ a subsequent bare init is a clean no-op.
    let out = carina(&["init", project_str]);
    assert!(
        out.status.success(),
        "init after a completed migration should succeed (lock matches).\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn force_required_to_overwrite_populated_target() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    let project_str = project.to_str().unwrap();

    write_project(project, "old.state.json");
    assert!(carina(&["init", project_str]).status.success());

    fs::write(project.join("old.state.json"), state_json("src-lineage", 1)).unwrap();
    // A *different* state already sits at the target address.
    fs::write(project.join("new.state.json"), state_json("dst-lineage", 1)).unwrap();

    write_project(project, "new.state.json");

    // Without --force: refuse, target untouched.
    let out = carina(&["init", "--migrate-state", project_str]);
    assert!(
        !out.status.success(),
        "migration into a populated target must fail without --force",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already contains state"),
        "expected populated-target refusal, got:\n{stderr}",
    );
    assert!(
        fs::read_to_string(project.join("new.state.json"))
            .unwrap()
            .contains("dst-lineage"),
        "target must be untouched on refusal",
    );

    // With --force: the target is overwritten by the source.
    let out = carina(&["init", "--migrate-state", "--force", project_str]);
    assert!(
        out.status.success(),
        "init --migrate-state --force should overwrite.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        fs::read_to_string(project.join("new.state.json"))
            .unwrap()
            .contains("src-lineage"),
        "target should now hold the source state",
    );
}
