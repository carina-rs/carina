use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::check_and_migrate;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    op_log: PathBuf,
}

impl Scenario {
    fn new(resource_count: usize) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let scenario = Self {
            mock_state: project.join("mock-state.json"),
            op_log: project.join("op.log"),
            project,
            _temp: temp,
        };
        scenario.write_resources(resource_count, "");
        assert_success("carina init", &scenario.carina(&["init", "."]));
        assert_success(
            "initial apply",
            &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
        );
        fs::write(&scenario.op_log, "").expect("clear operation log");
        scenario
    }

    fn resource_blocks(resource_count: usize) -> String {
        (1..=resource_count)
            .map(|index| {
                format!("let r{index} = mock.test.resource {{\n  name = \"r{index}\"\n}}\n")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn write_resources(&self, resource_count: usize, state_blocks: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                "backend local {{ path = \"carina.state.json\" }}\n\nprovider mock {{}}\n\n{}\n{state_blocks}",
                Self::resource_blocks(resource_count),
            ),
        )
        .expect("write configuration");
    }

    fn write_changed_resource(&self, change: &str, create_before_destroy: bool) {
        let directives = if create_before_destroy {
            "\n  directives {\n    create_before_destroy = true\n  }"
        } else {
            ""
        };
        fs::write(
            self.project.join("main.crn"),
            format!(
                "backend local {{ path = \"carina.state.json\" }}\n\nprovider mock {{}}\n\nlet r1 = mock.test.resource {{\n  name = \"r1\"\n  {change}{directives}\n}}\n"
            ),
        )
        .expect("write changed resource configuration");
    }

    fn write_removed(&self, identities: &[&str]) {
        let blocks = identities
            .iter()
            .map(|identity| format!("removed {{\n  from = mock.test.resource '{identity}'\n}}\n"))
            .collect::<Vec<_>>()
            .join("\n");
        self.write_resources(0, &blocks);
    }

    fn write_import(&self, identity: &str, identifier: &str) {
        self.write_resources(
            1,
            &format!(
                "import {{\n  to = mock.test.resource '{identity}'\n  id = '{identifier}'\n}}\n"
            ),
        );
    }

    fn write_data_source(&self) {
        fs::write(
            self.project.join("main.crn"),
            r#"backend local { path = "carina.state.json" }

provider mock {}

let roles = read mock.iam.Roles {
  name_regex = ".*"
}
"#,
        )
        .expect("write data-source configuration");
    }

    fn add_legacy_data_source_row(&self) {
        self.mutate_state(|state| {
            state["resources"] = serde_json::json!([{
                "resource_type": "iam.Roles",
                "identity": "roles",
                "provider": "mock",
                "current": {
                    "kind": "missing_identifier",
                    "attributes": { "names": ["old-role"] }
                },
                "binding": "roles"
            }]);
        });
    }

    fn add_identifier_bearing_data_source_address_row(&self) {
        self.mutate_state(|state| {
            state["resources"] = serde_json::json!([{
                "resource_type": "iam.Roles",
                "identity": "roles",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "managed-provider-id",
                    "attributes": { "names": ["old-managed-value"] }
                },
                "binding": "roles"
            }]);
        });
    }

    fn mutate_state(&self, mutate: impl FnOnce(&mut serde_json::Value)) {
        let path = self.project.join("carina.state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read state")).expect("decode state");
        mutate(&mut state);
        fs::write(
            path,
            serde_json::to_vec_pretty(&state).expect("encode state"),
        )
        .expect("write state");
    }

    fn clear_current_identifiers(&self) {
        self.mutate_state(|state| {
            for row in state["resources"]
                .as_array_mut()
                .expect("state resources must be an array")
            {
                let current = row["current"]
                    .as_object_mut()
                    .expect("state current instance must be an object");
                current.insert("kind".to_string(), serde_json::json!("missing_identifier"));
                current.remove("identifier");
            }
        });
    }

    fn state_resource_json(&self, index: usize) -> serde_json::Value {
        let state: serde_json::Value = serde_json::from_slice(
            &fs::read(self.project.join("carina.state.json")).expect("read state"),
        )
        .expect("decode state");
        state["resources"][index].clone()
    }

    fn make_deposed_only_with_empty_identifier(&self) {
        self.mutate_state(|state| {
            let row = &mut state["resources"][0];
            *row = serde_json::json!({
                "resource_type": "test.resource",
                "identity": "r1",
                "provider": "mock",
                "current": { "kind": "none" },
                "deposed": [{
                    "key": "broken-deposed",
                    "identifier": "",
                    "attributes": {},
                    "dependency_bindings": []
                }]
            });
        });
    }

    fn add_deposed_generation(&self, current_identifier: serde_json::Value) {
        self.mutate_state(|state| {
            let row = &mut state["resources"][0];
            let current = row["current"]
                .as_object_mut()
                .expect("state current instance must be an object");
            if current_identifier.is_null() || current_identifier.as_str() == Some("") {
                current.insert("kind".to_string(), serde_json::json!("missing_identifier"));
                current.remove("identifier");
            } else {
                current.insert("kind".to_string(), serde_json::json!("applied"));
                current.insert("identifier".to_string(), current_identifier);
            }
            row["deposed"] = serde_json::json!([{
                "key": "old-generation",
                "identifier": "mock-old-id",
                "attributes": { "name": "r1-old" },
                "dependency_bindings": []
            }]);
        });
    }

    fn carina(&self, args: &[&str]) -> Output {
        command(&self.project)
            .args(args)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.op_log)
            .output()
            .expect("run carina")
    }

    fn operations(&self) -> String {
        fs::read_to_string(&self.op_log).unwrap_or_default()
    }

    fn state_resource_count(&self) -> usize {
        let state = check_and_migrate(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read state"),
        )
        .expect("load state")
        .into_state();
        state.resources().len()
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
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_missing_identifier_failure(label: &str, output: &Output, identity: &str) {
    let text = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\n{text}"
    );
    assert!(
        text.contains(&format!("mock.test.resource.{identity}"))
            && text.contains("saved state has no provider identifier")
            && text.contains("`removed` block")
            && text.contains("`import` block"),
        "{label} did not report actionable missing-identifier context\n{text}",
    );
}

#[test]
fn managed_row_without_identifier_blocks_plan_and_apply_before_mutation() {
    let scenario = Scenario::new(1);
    scenario.clear_current_identifiers();

    for args in [
        &["plan", "."][..],
        &["plan", "--refresh=false", "."][..],
        &["apply", ".", "--auto-approve", "--lock=false"][..],
    ] {
        let output = scenario.carina(args);
        assert_missing_identifier_failure("managed-resource operation", &output, "r1");
    }
    assert_eq!(scenario.operations(), "", "provider mutation was invoked");
}

#[test]
fn updated_and_replaced_rows_without_identifiers_fail_from_loaded_state_before_mutation() {
    for (change, create_before_destroy) in [
        ("comment = \"changed\"", false),
        ("replacement_key = \"after\"", false),
        ("replacement_key = \"after\"", true),
    ] {
        let scenario = Scenario::new(1);
        scenario.clear_current_identifiers();
        scenario.write_changed_resource(change, create_before_destroy);

        let plan = scenario.carina(&["plan", "--refresh=false", "."]);
        assert_missing_identifier_failure("changed managed-resource plan", &plan, "r1");
        let apply = scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]);
        assert_missing_identifier_failure("changed managed-resource apply", &apply, "r1");
        assert_eq!(scenario.operations(), "", "provider mutation was invoked");
    }
}

#[test]
fn orphan_row_without_identifier_blocks_plan_and_destroy_before_mutation() {
    let scenario = Scenario::new(1);
    scenario.clear_current_identifiers();
    scenario.write_resources(0, "");

    for args in [
        &["plan", "."][..],
        &["plan", "--refresh=false", "."][..],
        &["apply", ".", "--auto-approve", "--lock=false"][..],
        &["destroy", ".", "--auto-approve", "--lock=false"][..],
    ] {
        let output = scenario.carina(args);
        assert_missing_identifier_failure("orphan operation", &output, "r1");
    }
    assert_eq!(scenario.operations(), "", "provider mutation was invoked");
}

#[test]
fn state_refresh_warns_and_preserves_orphan_without_identifier() {
    let scenario = Scenario::new(1);
    scenario.clear_current_identifiers();
    scenario.write_resources(0, "");

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("orphan state refresh", &refresh);
    let text = output_text(&refresh);
    assert!(
        text.contains("mock.test.resource.r1")
            && text.contains("saved state has no provider identifier")
            && text.contains("not refreshed"),
        "state refresh must warn about the preserved malformed orphan\n{text}",
    );
    assert_eq!(scenario.state_resource_count(), 1);
    assert_eq!(scenario.operations(), "", "provider mutation was invoked");
}

#[test]
fn state_refresh_preserves_declared_row_without_identifier_byte_for_byte() {
    let scenario = Scenario::new(1);
    scenario.mutate_state(|state| {
        let row = &mut state["resources"][0];
        let current = row["current"]
            .as_object_mut()
            .expect("state current instance must be an object");
        current.insert("kind".to_string(), serde_json::json!("missing_identifier"));
        current.remove("identifier");
        row["binding"] = serde_json::json!("legacy-binding-sentinel");
        row["current"]["attributes"]["legacy_only"] = serde_json::json!("must-survive");
    });
    let before = scenario.state_resource_json(0);

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("declared state refresh", &refresh);
    let text = output_text(&refresh);
    assert!(
        text.contains("mock.test.resource.r1")
            && text.contains("saved state has no provider identifier")
            && text.contains("row was preserved"),
        "state refresh must warn about the preserved malformed declared row\n{text}",
    );
    assert_eq!(scenario.state_resource_json(0), before);
    assert_eq!(scenario.operations(), "", "provider mutation was invoked");
}

#[test]
fn current_instance_content_with_deposed_generations_is_not_a_shell() {
    for current_identifier in [serde_json::Value::Null, serde_json::json!("")] {
        let scenario = Scenario::new(1);
        scenario.add_deposed_generation(current_identifier);

        for args in [
            &["plan", "--refresh=false", "."][..],
            &["apply", ".", "--auto-approve", "--lock=false"][..],
        ] {
            let output = scenario.carina(args);
            assert_missing_identifier_failure("current-plus-deposed operation", &output, "r1");
        }
        assert_eq!(scenario.operations(), "", "provider mutation was invoked");
    }
}

#[test]
fn typed_read_address_prunes_legacy_data_source_row_without_hiding_managed_corruption() {
    for saved_plan in [false, true] {
        let scenario = Scenario::new(1);
        scenario.write_data_source();
        scenario.add_legacy_data_source_row();

        let plan = if saved_plan {
            scenario.carina(&["plan", ".", "--out", "legacy-data-source.plan"])
        } else {
            scenario.carina(&["plan", "."])
        };
        assert_success("plan with legacy data-source artifact", &plan);
        let text = output_text(&plan);
        assert!(
            !text.contains("saved state has no provider identifier"),
            "typed read row must not surface as a managed orphan\n{text}"
        );

        let apply = if saved_plan {
            scenario.carina(&[
                "apply",
                "legacy-data-source.plan",
                "--auto-approve",
                "--lock=false",
            ])
        } else {
            scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"])
        };
        assert_success("apply with legacy data-source artifact", &apply);
        assert_eq!(scenario.state_resource_count(), 0);
    }
}

#[test]
fn managed_to_data_source_plan_keeps_orphan_delete_and_data_source_read() {
    let scenario = Scenario::new(1);
    scenario.write_data_source();
    scenario.add_identifier_bearing_data_source_address_row();

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("managed-to-data-source plan", &plan);
    let text = output_text(&plan);
    assert!(
        text.contains("<= mock.iam.Roles roles (data source)"),
        "plan must retain the data-source read\n{text}",
    );
    assert!(
        text.contains("- mock.iam.Roles roles"),
        "plan must retain the managed orphan delete\n{text}",
    );
    assert!(
        text.contains("Plan: 1 to read, 0 to add, 0 to change, 1 to destroy."),
        "plan summary must include the orphan delete\n{text}",
    );
}

#[test]
fn managed_to_data_source_state_refresh_keeps_key_spaces_separate() {
    let scenario = Scenario::new(1);
    scenario.write_data_source();
    scenario.add_identifier_bearing_data_source_address_row();

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("managed-to-data-source state refresh", &refresh);
    assert_eq!(
        scenario.state_resource_count(),
        0,
        "the mock reports the old managed orphan missing, so refresh should remove it",
    );
}

#[test]
fn managed_to_data_source_destroy_still_deletes_managed_orphan() {
    let scenario = Scenario::new(1);
    scenario.write_data_source();
    scenario.add_identifier_bearing_data_source_address_row();

    let destroy = scenario.carina(&[
        "destroy",
        "--refresh=false",
        "--auto-approve",
        "--lock=false",
        ".",
    ]);
    assert_success("managed-to-data-source destroy", &destroy);
    assert_eq!(scenario.state_resource_count(), 0);
    assert!(
        scenario.operations().contains("delete iam.Roles.roles"),
        "destroy must send the orphan delete to the provider",
    );
}

#[test]
fn removed_then_import_recovers_missing_identifier_with_and_without_refresh() {
    for no_refresh in [false, true] {
        let scenario = Scenario::new(1);
        scenario.clear_current_identifiers();
        scenario.write_removed(&["r1"]);

        let mut args = vec!["plan", "."];
        if no_refresh {
            args.push("--refresh=false");
        }
        assert_success("plan removed recovery", &scenario.carina(&args));
        assert_success(
            "apply removed recovery",
            &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
        );
        assert_eq!(scenario.state_resource_count(), 0);
        assert_eq!(
            scenario.operations(),
            "",
            "removed must not mutate provider state"
        );

        scenario.write_import("r1", "mock-id");
        let mut args = vec!["plan", "."];
        if no_refresh {
            args.push("--refresh=false");
        }
        assert_success("plan import recovery", &scenario.carina(&args));

        assert_success(
            "apply import recovery",
            &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
        );
        let state = check_and_migrate(
            &fs::read_to_string(scenario.project.join("carina.state.json")).expect("read state"),
        )
        .expect("load imported state")
        .into_state();
        let row = state
            .find_resource("mock", "test.resource", "r1")
            .expect("imported row must exist");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("mock-id")
        );
    }
}

#[test]
fn empty_import_identifier_fails_during_plan_without_provider_call() {
    let scenario = Scenario::new(1);
    scenario.write_removed(&["r1"]);
    assert_success(
        "remove state before invalid import",
        &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    fs::write(&scenario.op_log, "").expect("clear operation log");

    scenario.write_import("r1", "");
    let plan = scenario.carina(&["plan", "."]);
    let text = output_text(&plan);
    assert!(
        !plan.status.success()
            && text.contains("import id must be a non-empty provider identifier")
            && text.contains("`import` block"),
        "empty import identifier must be rejected while planning\n{text}",
    );
    assert_eq!(scenario.operations(), "", "provider was called");
    assert_eq!(scenario.state_resource_count(), 0);
}

#[test]
fn state_refresh_warns_and_preserves_deposed_generation_with_empty_identifier() {
    let scenario = Scenario::new(1);
    scenario.make_deposed_only_with_empty_identifier();

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("deposed state refresh", &refresh);
    let text = output_text(&refresh);
    assert!(
        text.contains("broken-deposed")
            && text.contains("has no provider identifier")
            && text.contains("generation was preserved")
            && text.contains("`removed` block"),
        "state refresh must warn and retain the malformed deposed generation\n{text}",
    );

    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(scenario.project.join("carina.state.json")).expect("read state"),
    )
    .expect("decode state");
    assert_eq!(state["resources"][0]["deposed"][0]["key"], "broken-deposed");
    assert_eq!(state["resources"][0]["deposed"][0]["identifier"], "");
}

#[test]
fn invalid_deposed_identifier_has_working_removed_recovery() {
    let scenario = Scenario::new(1);
    scenario.make_deposed_only_with_empty_identifier();
    scenario.write_resources(0, "");

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let text = output_text(&plan);
    assert!(
        !plan.status.success(),
        "broken deposed plan unexpectedly succeeded\n{text}"
    );
    assert!(
        text.contains("deposed generation")
            && text.contains("broken-deposed")
            && text.contains("`removed` block")
            && !text.contains("`import` block"),
        "deposed diagnostic must give a working recovery\n{text}",
    );

    let destroy = scenario.carina(&["destroy", ".", "--auto-approve", "--lock=false"]);
    let text = output_text(&destroy);
    assert!(
        !destroy.status.success()
            && text.contains("deposed generation")
            && text.contains("broken-deposed")
            && text.contains("`removed` block")
            && !text.contains("`import` block"),
        "destroy must report the deposed-specific recovery\n{text}",
    );

    scenario.write_removed(&["r1"]);
    let recovery_plan = scenario.carina(&["plan", ".", "--out", "deposed-recovery.plan"]);
    assert_success("save deposed removed recovery plan", &recovery_plan);
    let recovery_text = output_text(&recovery_plan);
    assert!(
        recovery_text.contains("broken-deposed")
            && !recovery_text.contains("provider identifier: \"\""),
        "the plan must show which malformed deposed generation leaves state\n{recovery_text}",
    );
    assert_success(
        "remove unrecoverable deposed generation",
        &scenario.carina(&[
            "apply",
            "deposed-recovery.plan",
            "--auto-approve",
            "--lock=false",
        ]),
    );
    assert_eq!(scenario.state_resource_count(), 0);
    assert_eq!(scenario.operations(), "", "provider mutation was invoked");

    // Pin the separate live-apply plan-construction call site: its removed
    // suppression must also run after deposed identifier errors are produced.
    let live_scenario = Scenario::new(1);
    live_scenario.make_deposed_only_with_empty_identifier();
    live_scenario.write_removed(&["r1"]);
    assert_success(
        "live apply deposed removed recovery",
        &live_scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    assert_eq!(live_scenario.state_resource_count(), 0);
    assert_eq!(
        live_scenario.operations(),
        "",
        "live removed recovery must not mutate provider state"
    );
}

#[test]
fn destroy_reports_every_row_without_identifier() {
    let scenario = Scenario::new(2);
    scenario.clear_current_identifiers();

    let destroy = scenario.carina(&["destroy", ".", "--auto-approve", "--lock=false"]);
    let text = output_text(&destroy);
    assert!(
        !destroy.status.success(),
        "destroy unexpectedly succeeded\n{text}"
    );
    assert!(text.contains("mock.test.resource.r1"), "{text}");
    assert!(text.contains("mock.test.resource.r2"), "{text}");
    assert!(text.contains("plan failed with 2 error(s)"), "{text}");
    assert_eq!(scenario.operations(), "", "provider mutation was invoked");
}
