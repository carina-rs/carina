//! Regression test for carina#3315.
//!
//! After carina#3283 improved the v6→current migration warning to promise
//! the operator that the upgrade "will be rewritten on the next
//! `carina apply` or `carina state refresh`", an apply with no
//! resource diff still left the on-disk state at v6: the no-op path
//! returned without calling the state writer. The warning then
//! re-emitted forever on every subsequent plan.
//!
//! The fix routes a migrated state load through a persist-on-load
//! step at the lock-held entry of `apply` / `destroy` /
//! `state refresh`, so the disk is upgraded the first time a
//! mutating command runs under the lock — independent of whether
//! that command has any resource or export work to do.
//!
//! This test pins the real binary behavior so a future refactor that
//! re-introduces a no-op short-circuit ahead of the persist step
//! fails loudly.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn plain_text_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    cmd
}

fn write_v6_state(dir: &std::path::Path) {
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

fn read_state_version_and_serial(path: &std::path::Path) -> (u32, u64) {
    let content = fs::read_to_string(path).expect("state file must exist");
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    let version = v["version"].as_u64().expect("version field") as u32;
    let serial = v["serial"].as_u64().expect("serial field");
    (version, serial)
}

#[test]
fn apply_persists_v6_to_current_with_no_resource_diff() {
    let dir = TempDir::new().unwrap();
    write_v6_state(dir.path());

    let state_path = dir.path().join("carina.state.json");
    let (before_version, _before_serial) = read_state_version_and_serial(&state_path);
    assert_eq!(before_version, 6, "test setup: state must start at v6");

    // Run apply --auto-approve against a project with no source-side
    // resources and a v6 state with no resources. The plan should be
    // "0 to add, 0 to change, 0 to destroy" (no resource diff, no
    // export diff), but the migrated state must still be persisted.
    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir.path())
        .args(["apply", "--auto-approve", "."])
        .output()
        .expect("failed to run carina apply");
    assert!(
        output.status.success(),
        "carina apply failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let (after_version, after_serial) = read_state_version_and_serial(&state_path);
    assert_eq!(
        after_version,
        carina_state::StateFile::CURRENT_VERSION,
        "apply must rewrite the on-disk state at the current schema version \
         after a v6→v{} in-memory migration. \
         stderr:\n{}\nstdout:\n{}",
        carina_state::StateFile::CURRENT_VERSION,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        after_serial > 1,
        "apply must advance serial when persisting a migration (was 1, now {after_serial})"
    );

    // A second apply against the now-current state must NOT emit the
    // migration warning (the disk is already at the current version).
    let output2 = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir.path())
        .args(["apply", "--auto-approve", "."])
        .output()
        .expect("failed to run second carina apply");
    assert!(output2.status.success());
    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    assert!(
        !stderr2.contains("is v6 on disk"),
        "second apply must not re-emit the migration warning. stderr:\n{stderr2}"
    );
}

#[test]
fn apply_persists_v6_to_current_under_no_lock_flag() {
    // Same scenario as `apply_persists_v6_to_current_with_no_resource_diff`
    // but with `--lock=false`. The dispatch in
    // `load_state_persist_if_migrated` must pick the unlocked writer
    // (`save_state_unlocked`) when no `LockInfo` is held, and the
    // disk still has to advance to the current schema. Caught the
    // round-2 review gap that the locked-mode tests alone did not
    // exercise the `lock: None` branch of the helper.
    let dir = TempDir::new().unwrap();
    write_v6_state(dir.path());

    let state_path = dir.path().join("carina.state.json");
    let (before_version, _) = read_state_version_and_serial(&state_path);
    assert_eq!(before_version, 6);

    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir.path())
        .args(["apply", "--auto-approve", "--lock=false", "."])
        .output()
        .expect("failed to run carina apply --lock=false");
    assert!(
        output.status.success(),
        "carina apply --lock=false failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let (after_version, _) = read_state_version_and_serial(&state_path);
    assert_eq!(
        after_version,
        carina_state::StateFile::CURRENT_VERSION,
        "apply --lock=false must still rewrite a migrated state"
    );
}

#[test]
fn destroy_persists_v6_to_current_when_state_has_no_resources() {
    // Sibling path of `apply`: `destroy` also takes the apply lock
    // and short-circuits ("No resources to destroy.") when the
    // state has no managed resources. The migration must still be
    // persisted before that short-circuit returns — otherwise an
    // operator that runs `apply` then `destroy` on a v6 directory
    // sees the warning re-emit forever from the destroy side too.
    let dir = TempDir::new().unwrap();
    write_v6_state(dir.path());

    let state_path = dir.path().join("carina.state.json");
    let (before_version, _) = read_state_version_and_serial(&state_path);
    assert_eq!(before_version, 6);

    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir.path())
        .args(["destroy", "--auto-approve", "."])
        .output()
        .expect("failed to run carina destroy");
    assert!(
        output.status.success(),
        "carina destroy failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let (after_version, _) = read_state_version_and_serial(&state_path);
    assert_eq!(
        after_version,
        carina_state::StateFile::CURRENT_VERSION,
        "destroy must rewrite the on-disk state at the current schema \
         version even when the no-resources short-circuit fires"
    );
}

#[test]
fn state_refresh_persists_v6_to_current_when_state_has_no_resources() {
    // `state refresh` returns early when the state has no resources
    // ("No resources in state. Nothing to refresh."). The migration
    // persistence must still happen on that path — otherwise a v6
    // state with empty resources would never be upgraded by
    // `carina state refresh` either, and the migration warning
    // would persist forever on directories that legitimately have
    // no managed resources yet.
    let dir = TempDir::new().unwrap();
    write_v6_state(dir.path());

    let state_path = dir.path().join("carina.state.json");
    let (before_version, _) = read_state_version_and_serial(&state_path);
    assert_eq!(before_version, 6);

    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir.path())
        .args(["state", "refresh", "."])
        .output()
        .expect("failed to run carina state refresh");
    assert!(
        output.status.success(),
        "carina state refresh failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let (after_version, _) = read_state_version_and_serial(&state_path);
    assert_eq!(
        after_version,
        carina_state::StateFile::CURRENT_VERSION,
        "state refresh must rewrite the on-disk state at the current \
         schema version even when the empty-resource short-circuit fires"
    );
}
