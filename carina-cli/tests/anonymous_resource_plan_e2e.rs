//! End-to-end regression coverage for carina#3826.
//!
//! Anonymous resources must never collapse while their identities are still
//! pending. Schema-stable and let-bound resources both reach the real plan;
//! multiple mutable-attribute-derived resources are rejected before planning.

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
        self.carina_with_schema(args, false)
    }

    fn carina_with_stable_schema(&self, args: &[&str]) -> Output {
        self.carina_with_schema(args, true)
    }

    fn carina_with_schema(&self, args: &[&str], stable_schema: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(args);
        if stable_schema {
            command.env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1");
        } else {
            command.env_remove("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA");
        }
        command.output().expect("run carina")
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

const LET_ALPHA_AND_BETA: &str = r#"let alpha = mock.test.resource {
  name = "alpha"
}

let beta = mock.test.resource {
  name = "beta"
}
"#;

const LET_ALPHA_AND_ANONYMOUS_BETA: &str = r#"let alpha = mock.test.resource {
  name = "alpha"
}

mock.test.resource {
  name = "beta"
}
"#;

const LET_ALPHA_AND_ANONYMOUS_BETA2: &str = r#"let alpha = mock.test.resource {
  name = "alpha"
}

mock.test.resource {
  name = "beta2"
}
"#;

const ATTRIBUTE_DERIVED_IDENTITY_ERROR: &str = "Anonymous resource identity is derived from \
mutable attributes for multiple 'mock.test.resource' declarations in the same scope (provider \
instance '<default>', module instance '<root>'): declaration #1 'mock.test.resource', declaration \
#2 'mock.test.resource'. Use `let` bindings to give them distinct stable identities.";

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
fn validate_rejects_multiple_attribute_derived_anonymous_resources() {
    let scenario = Scenario::new();

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let validate = scenario.carina(&["validate", "."]);
    let stderr = String::from_utf8_lossy(&validate.stderr);
    assert!(
        !validate.status.success() && stderr.contains(ATTRIBUTE_DERIVED_IDENTITY_ERROR),
        "validate must reject ambiguous attribute-derived anonymous identities\n\
         status: {}\nstdout:\n{}\nstderr:\n{stderr}",
        validate.status,
        String::from_utf8_lossy(&validate.stdout),
    );
}

#[test]
fn plan_rejects_multiple_attribute_derived_anonymous_resources() {
    let scenario = Scenario::new();

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let stderr = String::from_utf8_lossy(&plan.stderr);
    assert!(
        !plan.status.success() && stderr.contains(ATTRIBUTE_DERIVED_IDENTITY_ERROR),
        "plan must reject ambiguous attribute-derived anonymous identities\n\
         status: {}\nstdout:\n{}\nstderr:\n{stderr}",
        plan.status,
        String::from_utf8_lossy(&plan.stdout),
    );
}

#[test]
fn stable_anonymous_resources_both_reach_the_plan() {
    let scenario = Scenario::new();

    let init = scenario.carina_with_stable_schema(&["init", "."]);
    assert_success("carina init", &init);

    let plan =
        scenario.carina_with_stable_schema(&["plan", "--refresh=false", "--out", "plan.json", "."]);
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

    let init = scenario.carina_with_stable_schema(&["init", "."]);
    assert_success("carina init", &init);

    let apply = scenario.carina_with_stable_schema(&["apply", "--auto-approve", "."]);
    assert_success("initial carina apply", &apply);

    let plan = scenario.carina_with_stable_schema(&["plan", "."]);
    let plan_stdout = String::from_utf8_lossy(&plan.stdout);
    let plan_stderr = String::from_utf8_lossy(&plan.stderr);

    let destroy = scenario.carina_with_stable_schema(&["destroy", "--auto-approve", "."]);
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
fn claimed_named_row_is_not_adopted_by_the_single_attribute_derived_anonymous_resource() {
    let scenario = Scenario::new();
    scenario.write_main(LET_ALPHA_AND_ANONYMOUS_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);
    let first_apply = scenario.carina(&["apply", "--auto-approve", "."]);
    assert_success("apply alpha and beta", &first_apply);

    scenario.write_main(LET_ALPHA_AND_ANONYMOUS_BETA2);
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
            && plan_stdout.contains("Plan: 0 to add, 1 to change, 0 to destroy.")
            && apply.status.success()
            && !lock_remains
            && replan.status.success()
            && replan_stdout.contains("No changes")
            && state_rows == 2,
        "editing the sole anonymous resource must update it in place without claiming the named row\n\
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
fn saved_plan_apply_for_two_let_bound_resources_converges() {
    let scenario = Scenario::new();
    scenario.write_main(LET_ALPHA_AND_BETA);

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
        "saved-plan apply must preserve both let-bound resources and converge\n\
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
    scenario.write_main(ALPHA);

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
