//! Integration tests for the `plan-fixture` example binary.
//!
//! The example renders plan output from fixtures without needing real
//! provider plugins, mirroring the logic used by the plan snapshot tests.

use std::process::Command;

fn run_plan_fixture(args: &[&str]) -> std::process::Output {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["run", "--quiet", "--example", "plan-fixture", "--"])
        .args(args)
        .current_dir(manifest_dir);
    cmd.output()
        .expect("failed to execute plan-fixture example")
}

fn assert_success(output: &std::process::Output, fixture: &str) {
    assert!(
        output.status.success(),
        "plan-fixture for '{}' failed.\nstdout: {}\nstderr: {}",
        fixture,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !output.stdout.is_empty(),
        "plan-fixture for '{}' produced empty stdout",
        fixture,
    );
}

#[test]
fn plan_fixture_all_create() {
    let output = run_plan_fixture(&["all_create"]);
    assert_success(&output, "all_create");
}

#[test]
fn plan_fixture_mixed_operations() {
    let output = run_plan_fixture(&["mixed_operations"]);
    assert_success(&output, "mixed_operations");
}

#[test]
fn plan_fixture_delete_orphan() {
    let output = run_plan_fixture(&["delete_orphan"]);
    assert_success(&output, "delete_orphan");
}

#[test]
fn plan_fixture_compact_detail_none() {
    let output = run_plan_fixture(&["compact", "--detail", "none"]);
    assert_success(&output, "compact");
}

#[test]
fn plan_fixture_destroy_full() {
    let output = run_plan_fixture(&["destroy_full", "--destroy"]);
    assert_success(&output, "destroy_full");
}

#[test]
fn plan_fixture_upstream_state() {
    let output = run_plan_fixture(&["upstream_state"]);
    assert_success(&output, "upstream_state");
}

#[test]
fn plan_fixture_upstream_state_map_subscript() {
    let output = run_plan_fixture(&["upstream_state_map_subscript"]);
    assert_success(&output, "upstream_state_map_subscript");
}
