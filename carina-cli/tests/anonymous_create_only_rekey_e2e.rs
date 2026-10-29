//! End-to-end regression coverage for carina#3914.
//!
//! Replacing an anonymous resource after a create-only input changes must
//! persist the identity derived from the new inputs, not the old state key.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use carina_state::{ResourceState, StateFile, check_and_migrate};
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new(cluster_name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();

        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend configuration");

        let scenario = Self {
            _temp: temp,
            project,
        };
        scenario.write_main(cluster_name);
        scenario
    }

    fn write_main(&self, cluster_name: &str) {
        self.write_main_with_ref_directives_and_extra(cluster_name, "name", "", "");
    }

    fn write_main_with_prevent_destroy(&self, cluster_name: &str) {
        self.write_main_with_ref_directives_and_extra(
            cluster_name,
            "name",
            "  directives { prevent_destroy = true }\n",
            "",
        );
    }

    fn write_main_with_saved_plan_secret(&self, cluster_name: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"let roles = read mock.iam.Roles {{
  name_regex = "late"
}}

mock.test.resource {{
  name = "${{roles.names[0]}}-{cluster_name}"
  comment = secret("SAVED_PLAN_ANONYMOUS_MOVE_SECRET_3914")
}}
"#,
            ),
        )
        .expect("write saved-plan resource configuration");
    }

    fn seed_saved_plan_data_source(&self) {
        fs::write(
            self.project.join("mock-provider-state.json"),
            r#"{
  "version": 2,
  "states": {
    "iam.Roles.roles": {
      "name_regex": "late",
      "names": ["resolved-role"]
    }
  }
}
"#,
        )
        .expect("seed mock data source");
    }

    fn write_main_with_ref_directives_and_extra(
        &self,
        cluster_name: &str,
        cluster_attribute: &str,
        directives: &str,
        extra: &str,
    ) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"let cluster = mock.compute.Instance {{
  name = "cluster"
  cluster_name = "stable"
}}

mock.compute.Instance {{
  name = "web"
  cluster_name = "${{cluster.{cluster_attribute}}}-{cluster_name}"
{directives}}}
{extra}
"#,
            ),
        )
        .expect("write resource configuration");
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("CARINA_MOCK_STATE_FILE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn carina_with_saved_plan_data_source(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.project.join("mock-provider-state.json"),
            )
            .args(args)
            .output()
            .expect("run carina with saved-plan data source")
    }

    fn state(&self) -> StateFile {
        check_and_migrate(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read local state"),
        )
        .expect("load local state")
        .into_state()
    }

    fn anonymous_compute_instance(&self) -> ResourceState {
        let state = self.state();
        let rows: Vec<_> = state
            .resources()
            .iter()
            .filter(|row| {
                row.provider == "mock"
                    && row.resource_type == "compute.Instance"
                    && row.binding.is_none()
            })
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "state must contain exactly one anonymous mock.compute.Instance row: {:#?}",
            state.resources(),
        );
        rows[0].clone()
    }

    fn anonymous_test_resource(&self) -> ResourceState {
        let state = self.state();
        let rows: Vec<_> = state
            .resources()
            .iter()
            .filter(|row| {
                row.provider == "mock"
                    && row.resource_type == "test.resource"
                    && row.binding.is_none()
            })
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "state must contain exactly one anonymous mock.test.resource row: {:#?}",
            state.resources(),
        );
        rows[0].clone()
    }
}

#[test]
fn saved_plan_replays_late_anonymous_move_before_secret_verification() {
    let scenario = Scenario::new("c1");
    scenario.seed_saved_plan_data_source();
    scenario.write_main_with_saved_plan_secret("c1");
    assert_success(
        "saved late-move fixture init",
        &scenario.carina_with_saved_plan_data_source(&["init", "."]),
    );
    assert_success(
        "saved late-move fixture apply",
        &scenario.carina_with_saved_plan_data_source(&["apply", "--auto-approve"]),
    );
    let initial_identity = scenario
        .anonymous_test_resource()
        .identity
        .as_str()
        .to_string();

    scenario.write_main_with_saved_plan_secret("c2");
    let plan =
        scenario.carina_with_saved_plan_data_source(&["plan", "--out", "late-move.plan.json", "."]);
    assert_success("saved late-move plan", &plan);
    let apply = scenario.carina_with_saved_plan_data_source(&[
        "apply",
        "--auto-approve",
        "--lock=false",
        "late-move.plan.json",
    ]);
    assert_success("saved late-move apply", &apply);
    assert_ne!(
        scenario.anonymous_test_resource().identity.as_str(),
        initial_identity,
        "saved-plan apply must persist the late-recomputed anonymous identity"
    );
}

#[test]
fn destroy_honors_new_prevent_destroy_on_moved_anonymous_source() {
    let scenario = Scenario::new("c1");
    assert_success(
        "prevent-destroy fixture init",
        &scenario.carina(&["init", "."]),
    );
    assert_success(
        "prevent-destroy fixture apply",
        &scenario.carina(&["apply", "--auto-approve"]),
    );
    let initial_identity = scenario
        .anonymous_compute_instance()
        .identity
        .as_str()
        .to_string();

    scenario.write_main_with_prevent_destroy("c2");
    let destroy = scenario.carina(&["destroy", "--auto-approve", "."]);
    let rendered = output_text(&destroy);

    assert!(
        !destroy.status.success(),
        "prevent_destroy on the desired anonymous resource must refuse destroy\noutput:\n{rendered}"
    );
    assert!(
        rendered.contains("prevent_destroy"),
        "destroy must report the desired directive\noutput:\n{rendered}"
    );
    assert_eq!(
        scenario.anonymous_compute_instance().identity.as_str(),
        initial_identity,
        "the protected persisted source row must remain at its original address"
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstatus: {}\noutput:\n{}",
        output.status,
        output_text(output),
    );
}

#[test]
fn anonymous_create_only_replacement_rekeys_state_to_the_new_identity() {
    let changed = Scenario::new("c1");
    let changed_init = changed.carina(&["init", "."]);
    assert_success("changed project carina init", &changed_init);
    let initial_apply = changed.carina(&["apply", "--auto-approve"]);
    assert_success("initial carina apply", &initial_apply);
    let initial_row = changed.anonymous_compute_instance();
    let initial_identity = initial_row.identity.as_str().to_string();
    assert_eq!(
        initial_row.dependency_bindings,
        ["cluster".to_string()].into_iter().collect(),
        "the anonymous row must retain its dependency metadata before the move"
    );

    changed.write_main("c2");
    let refresh = changed.carina(&["state", "refresh", "."]);
    assert_success("state refresh with changed desired identity", &refresh);
    let refreshed_row = changed.anonymous_compute_instance();
    assert_eq!(
        refreshed_row.identity.as_str(),
        initial_identity,
        "state refresh must preserve the persisted anonymous address"
    );
    assert_eq!(
        refreshed_row.dependency_bindings,
        ["cluster".to_string()].into_iter().collect(),
    );

    let replacement_plan = changed.carina(&["plan"]);
    assert_success("replacement carina plan", &replacement_plan);
    let replacement_plan_output = output_text(&replacement_plan);
    assert!(
        replacement_plan_output
            .contains("Plan: 0 to add, 0 to change, 1 to replace, 0 to destroy, 1 to move."),
        "the create-only edit must plan one Replace plus its state move\noutput:\n{replacement_plan_output}",
    );
    assert!(
        replacement_plan_output.contains(&format!("(moved from: {initial_identity})")),
        "the Replace must be annotated with its old state address\noutput:\n{replacement_plan_output}",
    );

    let replacement_apply = changed.carina(&["apply", "--auto-approve"]);
    assert_success("replacement carina apply", &replacement_apply);
    let replacement_output = output_text(&replacement_apply);
    assert!(
        replacement_output
            .contains("Plan: 0 to add, 0 to change, 1 to replace, 0 to destroy, 1 to move."),
        "apply must retain the Replace plus move plan\noutput:\n{replacement_output}",
    );
    assert!(
        replacement_output.contains(&format!("(moved from: {initial_identity})")),
        "apply must retain the move annotation\noutput:\n{replacement_output}",
    );
    let replaced_row = changed.anonymous_compute_instance();
    let replaced_identity = replaced_row.identity.as_str().to_string();
    assert_eq!(
        replaced_row.dependency_bindings,
        ["cluster".to_string()].into_iter().collect(),
        "Move+Replace writeback must preserve dependency metadata on the re-keyed row"
    );

    let subsequent_plan = changed.carina(&["plan"]);
    assert_success("carina plan after replacement", &subsequent_plan);
    let subsequent_plan_output = output_text(&subsequent_plan);
    assert!(
        subsequent_plan_output.contains("No changes. Infrastructure is up-to-date."),
        "the replacement must converge\noutput:\n{subsequent_plan_output}",
    );

    let fresh = Scenario::new("c2");
    let fresh_init = fresh.carina(&["init", "."]);
    assert_success("fresh c2 project carina init", &fresh_init);
    let fresh_apply = fresh.carina(&["apply", "--auto-approve"]);
    assert_success("fresh c2 carina apply", &fresh_apply);
    let fresh_row = fresh.anonymous_compute_instance();
    let fresh_identity = fresh_row.identity.as_str().to_string();
    assert_eq!(
        fresh_row.dependency_bindings,
        ["cluster".to_string()].into_iter().collect(),
    );

    assert_eq!(
        replaced_identity, fresh_identity,
        "a replacement must persist the identity derived from its current create-only inputs; \
         initial identity: {initial_identity}"
    );
    assert_ne!(
        replaced_identity, initial_identity,
        "changing a create-only identity input must change the anonymous state identity"
    );

    let destroy = Scenario::new("c1");
    let destroy_init = destroy.carina(&["init", "."]);
    assert_success("destroy project carina init", &destroy_init);
    let destroy_apply = destroy.carina(&["apply", "--auto-approve"]);
    assert_success("destroy project initial apply", &destroy_apply);
    destroy.write_main("c2");
    let destroy_output = destroy.carina(&["destroy", "--auto-approve", "."]);
    assert_success("destroy with changed desired identity", &destroy_output);
    assert!(
        destroy.state().resources().is_empty(),
        "destroy must delete persisted rows without migrating anonymous addresses first"
    );
}
