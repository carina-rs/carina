//! End-to-end regression coverage for carina#3826.
//!
//! Two anonymous resources whose identities are still pending must both reach
//! the real plan. Historically dependency sorting keyed both resources as the
//! same empty identity and silently dropped the second declaration.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::Effect;
use carina_state::StateFile;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        copy_fixture(&project);
        Self {
            _temp: temp,
            project,
        }
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA")
            .args(args)
            .output()
            .expect("run carina")
    }
}

fn copy_fixture(destination: &Path) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anonymous_resource_plan");
    for entry in fs::read_dir(fixture).expect("read anonymous-resource fixture") {
        let entry = entry.expect("read fixture entry");
        fs::copy(entry.path(), destination.join(entry.file_name())).expect("copy fixture file");
    }
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn two_pending_anonymous_resources_both_reach_the_plan() {
    let scenario = Scenario::new();

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
    assert_success("carina plan", &plan);

    let stdout = String::from_utf8(plan.stdout).expect("plan stdout is UTF-8");
    assert!(
        stdout.contains("Plan: 2 to add, 0 to change, 0 to destroy."),
        "both anonymous resources must appear in the plan summary.\nstdout:\n{stdout}"
    );

    let saved_plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("plan.json")).expect("read saved plan"),
    )
    .expect("deserialize saved plan");
    let creates = saved_plan
        .plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Create(_)))
        .count();
    assert_eq!(creates, 2, "both anonymous creates must be serialized");
}

#[test]
fn plan_reports_how_to_repair_a_legacy_empty_identity_row() {
    let scenario = Scenario::new();

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let invalid_state = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "serial": 1,
        "lineage": "empty-identity-error-e2e",
        "carina_version": "older-carina",
        "resources": [
            {
                "resource_type": "test.resource",
                "identity": "",
                "provider": "mock",
                "identifier": "legacy-resource-123",
                "attributes": {}
            }
        ],
        "exports": {}
    });
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&invalid_state).expect("serialize invalid state fixture"),
    )
    .expect("write invalid state fixture");

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert!(
        !plan.status.success(),
        "carina plan must reject the empty identity row\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr),
    );

    let stderr = String::from_utf8(plan.stderr).expect("plan stderr is UTF-8");
    assert!(
        stderr.contains("resources[0]")
            && stderr.contains("provider=\"mock\"")
            && stderr.contains("resource_type=\"test.resource\"")
            && stderr.contains("identifier=\"legacy-resource-123\""),
        "error must identify the exact state row:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "Back up the state file, then remove this row from it. Run `carina plan`; the resource \
             that owned the row appears as a create with the identity Carina now assigns to it. \
             Put the row back with `identity` set to that value, keeping its `identifier` and \
             attributes, or leave it removed if the resource is no longer managed."
        ),
        "error must preserve the complete repair instruction:\n{stderr}"
    );
}
