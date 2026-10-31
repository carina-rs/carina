use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend configuration");
        fs::write(project.join("exports.crn"), "exports { ready = true }\n")
            .expect("write sibling configuration");

        Self {
            mock_state: project.join("mock-state.json"),
            project,
            _temp: temp,
        }
    }

    fn write_resource(&self, name: &str, explicit_opt_out: bool) {
        self.write_resource_with_prevent_destroy(name, explicit_opt_out.then_some(false));
    }

    fn write_resource_with_prevent_destroy(&self, name: &str, setting: Option<bool>) {
        let directives = setting.map_or_else(String::new, |value| {
            format!("  directives {{ prevent_destroy = {value} }}\n")
        });
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n{directives}}}\n"
            ),
        )
        .expect("write resource configuration");
    }

    fn remove_resource(&self) {
        fs::write(self.project.join("resources.crn"), "").expect("remove resource configuration");
    }

    fn write_resource_with_expression(&self, name: &str) {
        fs::write(
            self.project.join("flags.crn"),
            "let allow_destroy = false\n",
        )
        .expect("write sibling directive input");
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n  directives {{ prevent_destroy = allow_destroy }}\n}}\n"
            ),
        )
        .expect("write resource configuration with directive expression");
    }

    fn write_exports(&self, ready: bool) {
        fs::write(
            self.project.join("exports.crn"),
            format!("exports {{ ready = {ready} }}\n"),
        )
        .expect("write exports configuration");
    }

    fn state_json(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.project.join("carina.state.json")).expect("read local state"),
        )
        .expect("parse local state")
    }

    fn run(&self, args: &[&str]) -> Output {
        command(&self.project)
            .args(args)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("run carina")
    }
}

fn command(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstatus: {}\noutput:\n{}",
        output.status,
        output_text(output)
    );
}

fn assert_schema_default_refusal(label: &str, output: &Output) {
    let rendered = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\noutput:\n{rendered}"
    );
    assert!(
        rendered.contains("resource type is protected from destruction by default")
            && rendered.contains("directives { prevent_destroy = false }"),
        "{label} must name the schema default and exact opt-out\noutput:\n{rendered}"
    );
}

fn state_serial(state: &Value) -> u64 {
    state["serial"].as_u64().expect("state serial")
}

fn account_row(state: &Value) -> &Value {
    state["resources"]
        .as_array()
        .expect("state resources")
        .iter()
        .find(|row| row["identity"] == "account")
        .expect("account state row")
}

fn assert_persisted_prevent_destroy(state: &Value, value: bool) {
    assert_eq!(
        account_row(state)["directives"]["prevent_destroy"],
        json!({ "kind": "explicit", "value": value })
    );
}

#[test]
fn schema_default_protects_remove_replace_and_destroy_until_explicit_opt_out() {
    let scenario = Scenario::new();
    scenario.write_resource("account-v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.remove_resource();
    assert_schema_default_refusal("removed-resource plan", &scenario.run(&["plan", "."]));

    scenario.write_resource("account-v2", false);
    assert_schema_default_refusal("replacement plan", &scenario.run(&["plan", "."]));
    assert_schema_default_refusal(
        "destroy",
        &scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource("account-v2", true);
    assert_success("opted-out replacement plan", &scenario.run(&["plan", "."]));
    assert_success(
        "opted-out replacement apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.remove_resource();
    assert_success("opted-out removal plan", &scenario.run(&["plan", "."]));

    scenario.write_resource("account-v2", true);
    assert_success(
        "opted-out destroy",
        &scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]),
    );
}

#[test]
fn non_literal_false_is_valid_but_does_not_opt_out() {
    let scenario = Scenario::new();
    scenario.write_resource_with_expression("account-v1");
    assert_success("carina init", &scenario.run(&["init", "."]));
    let validate = scenario.run(&["validate", "."]);
    assert_success("carina validate", &validate);
    let validate_output = output_text(&validate);
    assert!(
        validate_output.contains("only a literal `false` opts out of schema-default protection"),
        "validate must warn about the ignored non-literal opt-out:\n{validate_output}"
    );
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource_with_expression("account-v2");
    let plan = scenario.run(&["plan", "."]);
    assert_schema_default_refusal("non-literal false replacement plan", &plan);
    let plan_output = output_text(&plan);
    assert!(
        plan_output.contains("only a literal `false` opts out of schema-default protection"),
        "plan must warn about the ignored non-literal opt-out:\n{plan_output}"
    );
}

#[test]
fn directive_only_false_is_persisted_before_removed_resource_delete() {
    let scenario = Scenario::new();
    scenario.write_resource("account-v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve"]),
    );
    let serial_before_directive_apply = state_serial(&scenario.state_json());

    scenario.write_resource_with_prevent_destroy("account-v1", Some(false));
    let directive_plan = scenario.run(&["plan", "."]);
    assert_success("directive-only opt-out plan", &directive_plan);
    let directive_plan_text = output_text(&directive_plan);
    assert!(
        !directive_plan_text.contains("No changes. Infrastructure is up-to-date.")
            && directive_plan_text.contains("mock.test.default_prevent_destroy.account")
            && directive_plan_text.contains("prevent_destroy: unspecified -> false"),
        "directive-only plan must render the affected address and field transition:\n{directive_plan_text}"
    );

    let json_plan = scenario.run(&["plan", "--json", "."]);
    assert_success("directive-only JSON plan", &json_plan);
    serde_json::from_slice::<Value>(&json_plan.stdout).expect("JSON plan stdout must remain valid");
    let json_stderr = String::from_utf8_lossy(&json_plan.stderr);
    assert!(
        json_stderr.contains("mock.test.default_prevent_destroy.account")
            && json_stderr.contains("prevent_destroy: unspecified -> false"),
        "versioned JSON plan keeps the detailed state-only section on stderr:\n{json_stderr}"
    );

    // Deliberately keep the default state lock enabled for the write-back.
    let directive_apply = scenario.run(&["apply", ".", "--auto-approve"]);
    assert_success("directive-only opt-out apply", &directive_apply);
    let after_directive_apply = scenario.state_json();
    assert_persisted_prevent_destroy(&after_directive_apply, false);
    let directive_serial = state_serial(&after_directive_apply);
    assert!(
        directive_serial > serial_before_directive_apply,
        "directive-only apply must advance the state serial"
    );

    let converged = scenario.run(&["apply", ".", "--auto-approve"]);
    assert_success("converged locked apply", &converged);
    assert!(
        output_text(&converged).contains("No changes needed."),
        "second apply must converge without another write:\n{}",
        output_text(&converged)
    );
    assert_eq!(
        state_serial(&scenario.state_json()),
        directive_serial,
        "converged apply must not advance the state serial"
    );

    scenario.remove_resource();
    assert_success("opted-out removal plan", &scenario.run(&["plan", "."]));
    assert_success(
        "opted-out removal apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
}

#[test]
fn saved_plan_apply_persists_directive_only_false() {
    let scenario = Scenario::new();
    scenario.write_resource("account-v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    let serial_before_directive_apply = state_serial(&scenario.state_json());

    scenario.write_resource_with_prevent_destroy("account-v1", Some(false));
    let saved_plan = scenario.run(&["plan", "--out", "directive-only.plan.json", "."]);
    assert_success("directive-only saved plan", &saved_plan);
    let saved_plan_text = output_text(&saved_plan);
    assert!(
        saved_plan_text.contains("mock.test.default_prevent_destroy.account")
            && saved_plan_text.contains("prevent_destroy: unspecified -> false"),
        "plan must disclose the exact state-only directive update:\n{saved_plan_text}"
    );
    assert_success(
        "directive-only saved-plan apply",
        &scenario.run(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "directive-only.plan.json",
        ]),
    );
    let after_directive_apply = scenario.state_json();
    assert_persisted_prevent_destroy(&after_directive_apply, false);
    assert!(
        state_serial(&after_directive_apply) > serial_before_directive_apply,
        "saved-plan directive-only apply must advance the state serial"
    );

    scenario.remove_resource();
    assert_success(
        "saved-plan opted-out removal plan",
        &scenario.run(&["plan", "."]),
    );
    assert_success(
        "saved-plan opted-out removal apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
}

#[test]
fn directive_only_true_is_persisted_as_explicit_protection() {
    let scenario = Scenario::new();
    scenario.write_resource("account-v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource_with_prevent_destroy("account-v1", Some(true));
    assert_success(
        "directive-only protection apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    assert_persisted_prevent_destroy(&scenario.state_json(), true);

    scenario.remove_resource();
    let plan = scenario.run(&["plan", "."]);
    let rendered = output_text(&plan);
    assert!(
        !plan.status.success(),
        "explicitly protected removal unexpectedly succeeded:\n{rendered}"
    );
    assert!(
        rendered.contains("resource has prevent_destroy set")
            && !rendered.contains("resource type is protected from destruction by default"),
        "the orphan must retain explicit-true provenance:\n{rendered}"
    );
}

#[test]
fn export_only_apply_preserves_resource_rows_byte_for_byte() {
    let scenario = Scenario::new();
    scenario.write_resource("account-v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    // Keep valid historical row metadata that is deliberately not reproduced
    // from current configuration. An export-only write must treat the entire
    // row as opaque instead of rebuilding it through the apply write-back
    // path.
    let mut seeded = scenario.state_json();
    let row = seeded["resources"]
        .as_array_mut()
        .expect("state resources")
        .iter_mut()
        .find(|row| row["identity"] == "account")
        .expect("account state row");
    row["dependency_bindings"] = json!(["historical-only"]);
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&seeded).expect("serialize seeded state"),
    )
    .expect("seed historical row metadata");

    let before = scenario.state_json();
    let resource_rows_before =
        serde_json::to_vec(&before["resources"]).expect("serialize resource rows before apply");
    let exports_before = before["exports"].clone();

    scenario.write_exports(false);
    let export_apply = scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]);
    assert_success("export-only apply", &export_apply);

    let after = scenario.state_json();
    let resource_rows_after =
        serde_json::to_vec(&after["resources"]).expect("serialize resource rows after apply");
    assert_eq!(
        resource_rows_after, resource_rows_before,
        "export-only apply must leave every persisted resource row byte-identical"
    );
    assert_ne!(
        after["exports"], exports_before,
        "the fixture must exercise an actual export change"
    );

    let mut normalized_before = before;
    normalized_before["serial"] = after["serial"].clone();
    normalized_before["exports"] = after["exports"].clone();
    assert_eq!(
        after, normalized_before,
        "only exports and the state serial may change on an export-only apply"
    );
}
