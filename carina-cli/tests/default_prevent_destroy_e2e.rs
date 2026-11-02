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

    fn write_resources_with_peer(&self, name: &str, setting: Option<bool>) {
        let directives = setting.map_or_else(String::new, |value| {
            format!("  directives {{ prevent_destroy = {value} }}\n")
        });
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n{directives}}}\n\nlet peer = mock.test.resource {{\n  name = \"peer\"\n}}\n"
            ),
        )
        .expect("write resource configuration with peer");
    }

    fn write_resources_with_dependent_peer(&self, name: &str) {
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n}}\n\nlet peer = mock.test.resource {{\n  name = \"peer\"\n  parent = account.name\n}}\n"
            ),
        )
        .expect("write protected resource with dependent peer");
    }

    fn write_resources_with_tagged_peer(&self, name: &str, setting: Option<bool>, peer_tag: &str) {
        let directives = setting.map_or_else(String::new, |value| {
            format!("  directives {{ prevent_destroy = {value} }}\n")
        });
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n{directives}}}\n\nlet peer = mock.test.resource {{\n  name = \"peer\"\n  tags = {{ revision = \"{peer_tag}\" }}\n}}\n"
            ),
        )
        .expect("write resource configuration with tagged peer");
    }

    fn write_tagged_peer(&self, tag: &str, force_delete: bool) {
        let directives = if force_delete {
            "  directives { force_delete = true }\n"
        } else {
            ""
        };
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let peer = mock.test.resource {{\n  name = \"peer\"\n  tags = {{ revision = \"{tag}\" }}\n{directives}}}\n"
            ),
        )
        .expect("write tagged peer resource");
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

fn prevent_destroy_change_json() -> Value {
    json!([{
        "resource_id": {
            "provider": "mock",
            "resource_type": "test.default_prevent_destroy",
            "identity": "account"
        },
        "fields": [{
            "name": "prevent_destroy",
            "from": { "kind": "unspecified" },
            "to": { "kind": "explicit", "value": false }
        }]
    }])
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
fn destroy_dependency_on_kept_prevent_destroy_target_reaches_typed_refusal() {
    let scenario = Scenario::new();
    scenario.write_resources_with_dependent_peer("account-v1");
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    let destroy = scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]);
    assert_schema_default_refusal("destroy with kept dependency target", &destroy);
    assert!(
        !output_text(&destroy).contains("Failed to analyze destroy dependencies"),
        "a dependency on an existing kept target is not missing:\n{}",
        output_text(&destroy)
    );
}

#[test]
fn folded_false_opts_out_of_schema_default() {
    let scenario = Scenario::new();
    scenario.write_resource_with_expression("account-v1");
    assert_success("carina init", &scenario.run(&["init", "."]));
    let validate = scenario.run(&["validate", "."]);
    assert_success("carina validate", &validate);
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource_with_expression("account-v2");
    assert_success(
        "folded false replacement plan",
        &scenario.run(&["plan", "."]),
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
    let json_plan_value = serde_json::from_slice::<Value>(&json_plan.stdout)
        .expect("JSON plan stdout must remain valid");
    assert_eq!(
        json_plan_value["persisted_directive_changes"],
        prevent_destroy_change_json(),
        "JSON plan output must expose the state-only directive change set"
    );
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
    let saved_plan_json: Value = serde_json::from_slice(
        &fs::read(scenario.project.join("directive-only.plan.json"))
            .expect("read directive-only saved plan"),
    )
    .expect("parse directive-only saved plan");
    assert_eq!(
        saved_plan_json["persisted_directive_changes"],
        prevent_destroy_change_json(),
        "saved plan must record the exact directive change set it displayed"
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
fn stale_saved_plan_refuses_a_different_directive_change_set() {
    let scenario = Scenario::new();
    scenario.write_resource_with_prevent_destroy("account-v1", None);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource_with_prevent_destroy("account-v1", Some(false));
    assert_success(
        "directive-changing saved plan",
        &scenario.run(&["plan", "--out", "stale.plan.json", "."]),
    );
    let recorded_plan: Value = serde_json::from_slice(
        &fs::read(scenario.project.join("stale.plan.json")).expect("read stale saved plan"),
    )
    .expect("parse stale saved plan");
    assert_eq!(
        recorded_plan["persisted_directive_changes"],
        prevent_destroy_change_json(),
        "the recorded side of the stale-plan comparison must be non-empty"
    );

    scenario.write_resource_with_prevent_destroy("account-v1", Some(true));
    assert_success(
        "intervening directive-only apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    assert_persisted_prevent_destroy(&scenario.state_json(), true);

    let stale_apply = scenario.run(&["apply", "--auto-approve", "--lock=false", "stale.plan.json"]);
    let rendered = output_text(&stale_apply);
    assert!(
        !stale_apply.status.success(),
        "a saved plan whose directive change set became stale must be refused:\n{rendered}"
    );
    assert!(
        rendered.contains("saved plan's resource directive changes no longer match")
            && rendered.contains("Recorded by the plan:")
            && rendered.contains("Recomputed from locked state:")
            && rendered.contains("true -> false")
            && rendered.contains("Re-run `carina plan --out"),
        "the typed refusal must explain the mismatch and tell the user to re-run plan:\n{rendered}"
    );
    assert_persisted_prevent_destroy(&scenario.state_json(), true);
}

#[test]
fn mixed_update_and_directive_change_appears_only_in_execution_plan() {
    let scenario = Scenario::new();
    scenario.write_tagged_peer("v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_tagged_peer("v2", true);
    let plan = scenario.run(&["plan", "."]);
    assert_success("mixed update plan", &plan);
    let rendered = output_text(&plan);
    assert!(
        rendered.contains("mock.test.resource peer")
            && !rendered.contains("State-only directive updates:"),
        "the updated resource must not be rendered a second time as state-only:\n{rendered}"
    );

    let json_plan = scenario.run(&["plan", "--json", "."]);
    assert_success("mixed update JSON plan", &json_plan);
    let json_plan: Value =
        serde_json::from_slice(&json_plan.stdout).expect("parse mixed update JSON plan");
    assert!(
        json_plan["plan"]["effects"]
            .as_array()
            .expect("saved plan effects")
            .iter()
            .any(|effect| effect.get("Update").is_some()),
        "the fixture must contain a provider Update effect"
    );
    assert_eq!(
        json_plan["persisted_directive_changes"],
        json!([]),
        "the same resource must not be duplicated in the state-only change set"
    );

    assert_success(
        "mixed update apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    let state = scenario.state_json();
    let peer = state["resources"]
        .as_array()
        .expect("state resources")
        .iter()
        .find(|row| row["identity"] == "peer")
        .expect("peer state row");
    assert_eq!(peer["directives"]["force_delete"], json!(true));
    assert_eq!(
        peer["current"]["attributes"]["tags"]["revision"],
        json!("v2")
    );
}

#[test]
fn saved_plan_with_provider_effect_and_directive_change_persists_both() {
    let scenario = Scenario::new();
    scenario.write_resources_with_tagged_peer("account-v1", None, "v1");
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resources_with_tagged_peer("account-v1", Some(false), "v2");
    assert_success(
        "mixed saved plan",
        &scenario.run(&["plan", "--out", "mixed.plan.json", "."]),
    );
    let saved_plan: Value = serde_json::from_slice(
        &fs::read(scenario.project.join("mixed.plan.json")).expect("read mixed saved plan"),
    )
    .expect("parse mixed saved plan");
    assert_eq!(
        saved_plan["persisted_directive_changes"],
        prevent_destroy_change_json(),
        "the saved plan must retain the directive-only resource beside provider effects"
    );
    assert!(
        saved_plan["plan"]["effects"]
            .as_array()
            .expect("saved plan effects")
            .iter()
            .any(|effect| effect.get("Update").is_some()),
        "the fixture must retain its provider effect"
    );

    let apply = scenario.run(&["apply", "--auto-approve", "--lock=false", "mixed.plan.json"]);
    assert_success("mixed saved-plan apply", &apply);
    let state = scenario.state_json();
    assert_persisted_prevent_destroy(&state, false);
    let peer = state["resources"]
        .as_array()
        .expect("state resources")
        .iter()
        .find(|row| row["identity"] == "peer")
        .expect("peer state row");
    assert_eq!(
        peer["current"]["attributes"]["tags"]["revision"],
        json!("v2"),
        "the provider effect must complete before final metadata writeback"
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
    row["dependency_bindings"] = json!([{
        "provider": "mock",
        "resource_type": "test.resource",
        "identity": "historical-only"
    }]);
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

#[test]
fn directive_only_apply_changes_only_directives_on_selected_rows() {
    let scenario = Scenario::new();
    scenario.write_resources_with_peer("account-v1", None);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    let mut seeded = scenario.state_json();
    for row in seeded["resources"].as_array_mut().expect("state resources") {
        row["dependency_bindings"] = json!([{
            "provider": "mock",
            "resource_type": "test.resource",
            "identity": "historical-only"
        }]);
    }
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&seeded).expect("serialize seeded state"),
    )
    .expect("seed historical row metadata");

    let before = scenario.state_json();
    scenario.write_resources_with_peer("account-v1", Some(false));
    let apply = scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]);
    assert_success("directive-only apply", &apply);
    let after = scenario.state_json();

    let row = |state: &Value, identity: &str| {
        state["resources"]
            .as_array()
            .expect("state resources")
            .iter()
            .find(|row| row["identity"] == identity)
            .unwrap_or_else(|| panic!("missing state row {identity}"))
            .clone()
    };
    let peer_before = row(&before, "peer");
    let peer_after = row(&after, "peer");
    assert_eq!(
        serde_json::to_vec(&peer_after).expect("serialize peer after"),
        serde_json::to_vec(&peer_before).expect("serialize peer before"),
        "rows outside the directive change set must remain byte-identical"
    );

    let account_before = row(&before, "account");
    let mut account_after = row(&after, "account");
    assert_ne!(
        account_after["directives"], account_before["directives"],
        "the selected row must persist the new directives"
    );
    account_after["directives"] = account_before["directives"].clone();
    assert_eq!(
        account_after, account_before,
        "the selected row may differ only in its directives"
    );
}
