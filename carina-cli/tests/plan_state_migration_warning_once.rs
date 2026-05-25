//! Regression test for carina#3283.
//!
//! `carina plan` reads the state file twice per run (T0 snapshot
//! followed by a post-plan drift re-read). Before the fix, the
//! state-schema migration warning was emitted directly from inside
//! `check_and_migrate` via `eprintln!`, so a single physical v6 state
//! file produced two (or three, once the refresh phase is also
//! involved) identical `Warning: Migrating state file...` lines per
//! run — noise that trained operators to ignore warnings.
//!
//! The fix moves the warning emission out of `check_and_migrate`,
//! which now returns the migration event as a typed `MigrationInfo`
//! value, and into the backend impls, where a per-instance
//! `OnceLock<MigrationInfo>` guarantees at most one log per
//! backend per process. This test pins the contract on the real
//! `carina plan` binary so a future refactor that re-introduces the
//! library-level `eprintln!` (or otherwise loses the dedupe) fails
//! loudly.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn plain_text_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    cmd
}

/// Write a minimal local-backend project plus a hand-authored v6
/// state file so the very next `carina plan` triggers a v6 → v7
/// in-memory migration.
fn init_project_with_v6_state(dir: &std::path::Path) {
    fs::write(
        dir.join("main.crn"),
        r#"backend local { path = "carina.state.json" }
exports { region: String = "ap-northeast-1" }"#,
    )
    .unwrap();

    let state = serde_json::json!({
        "version": 6,
        "serial": 1,
        "lineage": "11111111-1111-1111-1111-111111111111",
        "carina_version": "0.0.0-test",
        "resources": [],
        "exports": { "region": "ap-northeast-1" }
    });
    fs::write(
        dir.join("carina.state.json"),
        serde_json::to_string_pretty(&state).unwrap(),
    )
    .unwrap();

    let status = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("failed to execute carina init");
    assert!(status.success(), "carina init failed");
}

#[test]
fn plan_emits_migration_warning_exactly_once_for_v6_state_file() {
    let dir = TempDir::new().unwrap();
    init_project_with_v6_state(dir.path());

    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir.path())
        .args(["plan", "."])
        .output()
        .expect("failed to run carina plan");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The warning is the new wording (carina#3283): mentions the
    // on-disk version, the in-memory upgrade target, and the
    // operator's actionable next step. Match on the stable prefix
    // so a copy-edit doesn't break the test.
    let occurrences = stderr.matches("is v6 on disk").count();
    assert_eq!(
        occurrences, 1,
        "expected exactly one migration warning per `carina plan` run, \
         got {occurrences}. Full stderr:\n{stderr}\nstdout:\n{stdout}"
    );

    // Sanity-check that the old wording — which would re-appear if
    // a future change put the eprintln! back inside `check_and_migrate`
    // — is gone.
    assert!(
        !stderr.contains("Migrating state file from v"),
        "old past-tense wording leaked back into the warning. \
         stderr:\n{stderr}"
    );

    // Pin the *actionable* parts of the warning so a future copy-edit
    // that strips the operator's next-step guidance fails loudly: the
    // user needs to know (a) the in-memory target version, and (b) at
    // least one command that will rewrite disk state. Target version
    // is pulled from `StateFile::CURRENT_VERSION` so the test does
    // not break when a future schema bump lands.
    let current_version_token = format!("v{}", carina_state::StateFile::CURRENT_VERSION);
    assert!(
        stderr.contains(&current_version_token),
        "warning must name the target version ({current_version_token}). stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("state refresh") || stderr.contains("apply"),
        "warning must point operators at an actionable command \
         (carina apply / carina state refresh). stderr:\n{stderr}"
    );

    // Identify *which* file is stale (matters when upstream_state
    // chains read multiple state files in one process). The state
    // file path is fixed by `init_project_with_v6_state`.
    assert!(
        stderr.contains("carina.state.json"),
        "warning must name the affected state file. stderr:\n{stderr}"
    );
}
