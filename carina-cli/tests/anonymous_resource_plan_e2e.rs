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
use carina_core::hint::ProjectCommand;
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

    fn write_main(&self, resources: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                "backend local {{ path = \"carina.state.json\" }}\n\nprovider mock {{}}\n\n{resources}"
            ),
        )
        .expect("write project configuration");
    }

    fn state(&self) -> StateFile {
        carina_state::check_and_migrate(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read local state"),
        )
        .expect("load local state")
        .into_state()
    }
}

const ALPHA: &str = r#"mock.test.resource {
  name = "alpha"
}
"#;

const ALPHA_AND_BETA: &str = r#"mock.test.resource {
  name = "alpha"
}

mock.test.resource {
  name = "beta"
}
"#;

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
fn apply_two_anonymous_resources_then_plan_is_clean_and_destroy_removes_both() {
    let scenario = Scenario::new();
    scenario.write_main(ALPHA_AND_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let apply = scenario.carina(&["apply", "--auto-approve", "."]);
    assert_success("initial carina apply", &apply);

    let plan = scenario.carina(&["plan", "."]);
    let plan_stdout = String::from_utf8_lossy(&plan.stdout);
    let plan_stderr = String::from_utf8_lossy(&plan.stderr);

    let destroy = scenario.carina(&["destroy", "--auto-approve", "."]);
    let state = scenario.state();

    assert!(
        plan.status.success()
            && plan_stdout.contains("No changes")
            && destroy.status.success()
            && state.resources().is_empty(),
        "apply must converge and destroy must remove both rows\n\
         plan stdout:\n{plan_stdout}\nplan stderr:\n{plan_stderr}\n\
         destroy stdout:\n{}\ndestroy stderr:\n{}\nremaining rows: {}",
        String::from_utf8_lossy(&destroy.stdout),
        String::from_utf8_lossy(&destroy.stderr),
        state.resources().len(),
    );
}

#[test]
fn adding_second_anonymous_resource_preserves_first_and_releases_apply_lock() {
    let scenario = Scenario::new();
    scenario.write_main(ALPHA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);
    let first_apply = scenario.carina(&["apply", "--auto-approve", "."]);
    assert_success("apply alpha", &first_apply);

    scenario.write_main(ALPHA_AND_BETA);
    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let apply = scenario.carina(&["apply", "--auto-approve", "."]);
    let lock_path = scenario.project.join("carina.state.lock");
    let lock_remains = lock_path.exists();
    let replan = scenario.carina(&["plan", "."]);

    let plan_stdout = String::from_utf8_lossy(&plan.stdout);
    let replan_stdout = String::from_utf8_lossy(&replan.stdout);
    let state_rows = scenario.state().resources().len();
    assert!(
        plan.status.success()
            && plan_stdout.contains("Plan: 1 to add, 0 to change, 0 to destroy.")
            && apply.status.success()
            && !lock_remains
            && replan.status.success()
            && replan_stdout.contains("No changes")
            && state_rows == 2,
        "adding beta must create only beta, apply cleanly, and converge\n\
         plan stdout:\n{plan_stdout}\nplan stderr:\n{}\n\
         apply status: {}\napply stdout:\n{}\napply stderr:\n{}\n\
         lock remains: {lock_remains}\nreplan stdout:\n{replan_stdout}\n\
         replan stderr:\n{}\nstate rows: {state_rows}",
        String::from_utf8_lossy(&plan.stderr),
        apply.status,
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr),
        String::from_utf8_lossy(&replan.stderr),
    );
}

#[test]
fn saved_plan_apply_for_two_anonymous_resources_converges() {
    let scenario = Scenario::new();
    scenario.write_main(ALPHA_AND_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);
    let plan = scenario.carina(&["plan", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);
    let apply = scenario.carina(&["apply", "--auto-approve", "plan.json"]);
    let replan = scenario.carina(&["plan", "."]);

    let plan_stdout = String::from_utf8_lossy(&plan.stdout);
    let replan_stdout = String::from_utf8_lossy(&replan.stdout);
    let state_rows = scenario.state().resources().len();
    assert!(
        plan_stdout.contains("Plan: 2 to add, 0 to change, 0 to destroy.")
            && apply.status.success()
            && replan.status.success()
            && replan_stdout.contains("No changes")
            && state_rows == 2,
        "saved-plan apply must preserve both anonymous resources and converge\n\
         plan stdout:\n{plan_stdout}\nplan stderr:\n{}\n\
         apply status: {}\napply stdout:\n{}\napply stderr:\n{}\n\
         replan stdout:\n{replan_stdout}\nreplan stderr:\n{}\nstate rows: {state_rows}",
        String::from_utf8_lossy(&plan.stderr),
        apply.status,
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr),
        String::from_utf8_lossy(&replan.stderr),
    );
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
    let project_path = scenario
        .project
        .canonicalize()
        .expect("canonicalize temporary project");
    let state_path = project_path.join("carina.state.json");
    let plan_command = ProjectCommand::new("plan", &project_path);
    assert!(
        stderr.contains("resources[0]")
            && stderr.contains("provider=\"mock\"")
            && stderr.contains("resource_type=\"test.resource\"")
            && stderr.contains("identifier=\"legacy-resource-123\"")
            && stderr.contains(&state_path.display().to_string()),
        "error must identify the exact state row:\n{stderr}"
    );
    assert!(
        !stderr.contains("Failed to parse state file")
            && stderr.contains(&format!(
                "Back up the state file, then remove this row. Run `{plan_command}`; the resource \
                 that owned it appears as a create with its newly assigned identity. Put the row \
                 back with `identity` set to that value (keep its `identifier` and attributes), \
                 or leave it removed if the resource is no longer managed."
            )),
        "error must preserve the complete repair instruction:\n{stderr}"
    );
}
