//! Regressions for data-source reads under `--refresh=false` (carina#3993).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
}

impl Scenario {
    fn from_fixture(name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/data_source_refresh_false")
            .join(name);
        for entry in fs::read_dir(&fixture).expect("read fixture directory") {
            let entry = entry.expect("read fixture entry");
            fs::copy(entry.path(), project.join(entry.file_name())).expect("copy fixture file");
        }
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            operation_log: project.join("mock-operations.log"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        assert_success(
            "initial apply",
            &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
        );
        scenario.clear_operation_log();
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn clear_operation_log(&self) {
        fs::write(&self.operation_log, "").expect("clear mock operation log");
    }

    fn write_project_file(&self, name: &str, contents: &str) {
        fs::write(self.project.join(name), contents).expect("write project fixture");
    }

    fn operations(&self) -> String {
        fs::read_to_string(&self.operation_log).unwrap_or_default()
    }

    fn applied_identifiers(&self) -> BTreeMap<String, String> {
        let state: Value = serde_json::from_slice(
            &fs::read(self.project.join("carina.state.json")).expect("read Carina state"),
        )
        .expect("parse Carina state");
        state["resources"]
            .as_array()
            .expect("state resources")
            .iter()
            .filter_map(|resource| {
                let current = &resource["current"];
                (current["kind"] == "applied").then(|| {
                    (
                        resource["identity"]
                            .as_str()
                            .expect("resource identity")
                            .to_string(),
                        current["identifier"]
                            .as_str()
                            .expect("provider identifier")
                            .to_string(),
                    )
                })
            })
            .collect()
    }

    fn state_json(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.project.join("carina.state.json")).expect("read Carina state"),
        )
        .expect("parse Carina state")
    }

    fn replace_mock_data_source_names(&self, key: &str, names: &[&str]) {
        let mut state: Value =
            serde_json::from_slice(&fs::read(&self.mock_state).expect("read mock-provider state"))
                .expect("parse mock-provider state");
        state["states"][key]["names"] = Value::Array(
            names
                .iter()
                .map(|name| Value::String((*name).into()))
                .collect(),
        );
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&state).expect("serialize mock-provider state"),
        )
        .expect("update mock-provider state");
    }
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

fn assert_clean_data_source_plan(label: &str, output: &Output, expected_reads: usize) {
    assert_success(label, output);
    let rendered = output_text(output);
    let expected_summary =
        format!("Plan: {expected_reads} to read, 0 to add, 0 to change, 0 to destroy.");
    assert!(
        rendered.contains(&expected_summary),
        "{label} must have no managed-resource changes after reading the data source:\n{rendered}",
    );
    assert!(
        !rendered.contains("forces replacement") && !rendered.contains("still unknown at dispatch"),
        "{label} retained an unresolved data-source value:\n{rendered}",
    );
}

fn assert_refresh_false_plan_and_saved_apply(fixture: &str, expected_resources: usize) {
    let scenario = Scenario::from_fixture(fixture);
    let identifiers_before = scenario.applied_identifiers();
    assert_eq!(identifiers_before.len(), expected_resources);

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_clean_data_source_plan("carina plan --refresh=false", &plan, 1);
    assert!(
        scenario.operations().contains("read iam.Roles.roles"),
        "--refresh=false must still query data sources:\n{}",
        scenario.operations(),
    );

    scenario.clear_operation_log();
    let saved_plan = scenario.carina(&[
        "plan",
        "--refresh=false",
        "--out",
        "refresh-false.plan",
        ".",
    ]);
    assert_clean_data_source_plan("carina plan --refresh=false --out", &saved_plan, 1);
    assert!(
        scenario.operations().contains("read iam.Roles.roles"),
        "saved planning must query data sources under --refresh=false:\n{}",
        scenario.operations(),
    );

    scenario.clear_operation_log();
    let apply = scenario.carina(&[
        "apply",
        "--auto-approve",
        "--lock=false",
        "refresh-false.plan",
    ]);
    assert_success("carina apply saved plan", &apply);
    assert_eq!(
        scenario.applied_identifiers(),
        identifiers_before,
        "saved-plan apply must preserve the existing resource identities",
    );
    assert!(
        !scenario.operations().contains("delete test.resource"),
        "saved-plan apply must not delete a resource whose desired value came from a data source:\n{}",
        scenario.operations(),
    );
}

#[test]
fn refresh_false_reads_indexed_data_source_for_live_and_saved_plans() {
    assert_refresh_false_plan_and_saved_apply("indexed", 1);
}

#[test]
fn refresh_false_reads_data_source_before_expanding_for_each() {
    assert_refresh_false_plan_and_saved_apply("for_each", 2);
}

#[test]
fn state_refresh_reads_data_source_before_expanding_for_each() {
    let scenario = Scenario::from_fixture("for_each");
    let identifiers_before = scenario.applied_identifiers();
    assert_eq!(identifiers_before.len(), 2);
    scenario.clear_operation_log();

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("state refresh with data-source for iterable", &refresh);
    assert!(
        scenario.operations().contains("read iam.Roles.roles"),
        "state refresh must read the data source before expanding its for iterable:\n{}",
        scenario.operations(),
    );
    assert_eq!(scenario.applied_identifiers(), identifiers_before);
}

#[test]
fn state_refresh_retries_data_source_after_deferred_for_child_expands() {
    let scenario = Scenario::from_fixture("deferred_child");
    let mut state = scenario.state_json();
    let child = state["resources"]
        .as_array_mut()
        .expect("state resources")
        .iter_mut()
        .find(|resource| resource["identity"] == "kids[0]")
        .expect("expanded child state row");
    child["binding"] = Value::Null;
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).expect("serialize Carina state"),
    )
    .expect("simulate a legacy child row without binding metadata");
    scenario.write_project_file(
        "selected.crn",
        r#"let selected = read mock.iam.Roles {
  name_regex = kids[0].name
}
"#,
    );
    scenario.write_project_file(
        "exports.crn",
        r#"exports {
  selected_name: String = selected.names[0]
}
"#,
    );
    scenario.replace_mock_data_source_names("iam.Roles.selected", &["after"]);
    scenario.clear_operation_log();

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("state refresh with child-backed data source", &refresh);
    assert!(
        scenario.operations().contains("read iam.Roles.selected"),
        "state refresh must retry the read after the child exists:\n{}",
        scenario.operations(),
    );
    assert_eq!(
        scenario.state_json().pointer("/exports/selected_name"),
        Some(&Value::String("after".into())),
        "the refreshed data-source output must feed export writeback",
    );
}

#[test]
fn state_refresh_retries_data_source_after_upstream_data_source_read() {
    let scenario = Scenario::from_fixture("chained");

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("state refresh with chained data sources", &refresh);
    let operations = scenario.operations();
    assert!(
        operations.contains("read iam.Roles.roles"),
        "state refresh must read the upstream query:\n{operations}",
    );
    assert!(
        operations.contains("read iam.Roles.roles_again"),
        "state refresh must retry the downstream query with the upstream outcome:\n{operations}",
    );
}

#[test]
fn saved_plan_preserves_data_source_deferred_by_another_data_source() {
    let scenario = Scenario::from_fixture("chained");
    let identifiers_before = scenario.applied_identifiers();
    assert_eq!(identifiers_before.len(), 1);

    for (plan_name, refresh_arg) in [
        ("chained.plan", None),
        ("chained-refresh-false.plan", Some("--refresh=false")),
    ] {
        scenario.clear_operation_log();
        let mut plan_args = vec!["plan"];
        if let Some(refresh_arg) = refresh_arg {
            plan_args.push(refresh_arg);
        }
        plan_args.extend(["--out", plan_name, "."]);
        let plan = scenario.carina(&plan_args);
        assert_clean_data_source_plan("saved chained-data-source plan", &plan, 2);
        let plan_operations = scenario.operations();
        assert!(
            plan_operations.contains("read iam.Roles.roles")
                && plan_operations.contains("read iam.Roles.roles_again"),
            "planning must read both levels of the data-source chain:\n{plan_operations}",
        );

        scenario.clear_operation_log();
        let apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", plan_name]);
        assert_success("apply saved chained-data-source plan", &apply);
        assert_eq!(scenario.applied_identifiers(), identifiers_before);
        let operations = scenario.operations();
        assert!(
            !operations.contains("delete test.resource"),
            "saved-plan apply must not delete the chained data-source consumer:\n{operations}",
        );
        assert!(
            !operations.contains("create test.resource"),
            "saved-plan apply must not recreate the chained data-source consumer:\n{operations}",
        );
    }
}

#[test]
fn destroy_does_not_read_unused_data_sources() {
    for refresh_arg in [None, Some("--refresh=false")] {
        let scenario = Scenario::from_fixture("indexed");
        let mut args = vec!["destroy", "--auto-approve", "--lock=false"];
        if let Some(refresh_arg) = refresh_arg {
            args.push(refresh_arg);
        }
        args.push(".");
        let destroy = scenario.carina(&args);
        let label = refresh_arg.map_or("carina destroy", |_| "carina destroy --refresh=false");
        assert_success(label, &destroy);
        assert!(
            !scenario.operations().contains("read iam.Roles.roles"),
            "{label} must not query data sources whose values it discards:\n{}",
            scenario.operations(),
        );
    }
}
