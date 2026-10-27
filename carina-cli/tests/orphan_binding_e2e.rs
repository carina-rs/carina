use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        Self {
            mock_state: project.join("mock-state.json"),
            operation_log: project.join("mock-operations.log"),
            project,
            _temp: temp,
        }
    }

    fn write_main(&self, resource: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                "backend local {{ path = \"carina.state.json\" }}\n\nprovider mock {{}}\n\n{resource}"
            ),
        )
        .expect("write project configuration");
    }

    fn write_file(&self, name: &str, contents: &str) {
        fs::write(self.project.join(name), contents).expect("write project configuration");
    }

    fn carina(&self, args: &[&str]) -> Output {
        command(&self.project)
            .args(args)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
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

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn refreshed_orphan_delete_keeps_let_binding_in_plan() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let retired = mock.test.resource {
  name = "retired-resource"
}
"#,
    );

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    scenario.write_main("");
    let plan = scenario.carina(&["plan", "."]);
    assert_success("orphan delete plan", &plan);
    let output = output_text(&plan);
    assert!(
        output.contains("- mock.test.resource retired")
            && output.contains("Plan: 0 to add, 0 to change, 1 to destroy."),
        "refreshed orphan delete must be displayed under its let binding\nstdout/stderr:\n{output}"
    );
}

#[test]
fn state_refresh_keeps_orphan_binding_and_prevent_destroy() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let retired = mock.test.resource {
  name = "retired-resource"
  directives {
    prevent_destroy = true
  }
}
"#,
    );

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    scenario.write_main("");
    assert_success(
        "orphan state refresh",
        &scenario.carina(&["state", "refresh", "--lock=false", "."]),
    );

    let plan = scenario.carina(&["plan", "."]);
    let output = output_text(&plan);
    assert!(
        !plan.status.success(),
        "prevent_destroy must still block the refreshed orphan deletion\nstdout/stderr:\n{output}"
    );
    assert!(
        output.contains("prevent_destroy") && output.contains("retired"),
        "the refusal must identify the let-bound orphan and its preserved directive\nstdout/stderr:\n{output}"
    );
}

#[test]
fn state_refresh_does_not_apply_pending_moved_block_address_transitions() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let a = mock.test.resource {
  name = "a"
}
"#,
    );

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_main(
        r#"moved {
  from = mock.test.resource 'a'
  to   = mock.test.resource 'b'
}

let b = mock.test.resource {
  name = "a"
}
"#,
    );
    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("state refresh with pending move", &refresh);
    let output = output_text(&refresh);
    assert!(
        output.contains("State refreshed: 0 resources updated, 1 resource unchanged."),
        "state refresh must refresh the existing from-address without applying the move\n\
         stdout/stderr:\n{output}",
    );
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("carina.state.json"))
            .expect("read refreshed state"),
    )
    .expect("parse refreshed state");
    let resources = state["resources"].as_array().expect("state resources");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["identity"], "a");
}

#[test]
fn state_refresh_reads_data_source_against_orphan_binding() {
    let scenario = Scenario::new();
    scenario.write_file(
        "backend.crn",
        "backend local { path = \"carina.state.json\" }\n",
    );
    scenario.write_file("providers.crn", "provider mock {}\n");
    scenario.write_file(
        "main.crn",
        r#"let retired = mock.test.resource {
  name = "retired-resource"
}
"#,
    );

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    scenario.write_file("main.crn", "");
    scenario.write_file(
        "data.crn",
        r#"let lookup = read mock.iam.Roles {
  name_regex = retired.name
}
"#,
    );
    fs::write(&scenario.operation_log, "").expect("clear operation log");

    let refresh = scenario.carina(&["state", "refresh", "--lock=false", "."]);
    assert_success("state refresh with orphan-backed data source", &refresh);
    let operations = fs::read_to_string(&scenario.operation_log).unwrap_or_default();
    assert!(
        operations.contains("read iam.Roles.lookup"),
        "state refresh must resolve the orphan binding and dispatch the data source\n\
         operations:\n{operations}\noutput:\n{}",
        output_text(&refresh),
    );
}

#[test]
fn configured_binding_wins_over_same_binding_orphan_for_data_source_reads() {
    let scenario = Scenario::new();
    scenario.write_file(
        "backend.crn",
        "backend local { path = \"carina.state.json\" }\n",
    );
    scenario.write_file("providers.crn", "provider mock {}\n");
    scenario.write_file(
        "main.crn",
        r#"let bucket = mock.test.resource {
  name = "old-bucket"
}
"#,
    );
    fs::write(
        &scenario.mock_state,
        r#"{"version":2,"states":{"iam.Roles.lookup":{"name_regex":"old-bucket","names":["old"]}}}"#,
    )
    .expect("seed mock data source");

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", ".", "--auto-approve", "--lock=false"]),
    );
    let mut mock_state: serde_json::Value =
        serde_json::from_slice(&fs::read(&scenario.mock_state).expect("read mock-provider state"))
            .expect("parse mock-provider state");
    mock_state["states"]["test.resource.bucket"]["generated_rules"] =
        serde_json::json!([{ "action": "old-action" }]);
    fs::write(
        &scenario.mock_state,
        serde_json::to_vec_pretty(&mock_state).expect("serialize mock-provider state"),
    )
    .expect("seed an old-type-only refreshed attribute");
    scenario.write_file(
        "main.crn",
        r#"let bucket = mock.test.deferred_source {
  name = "new-bucket"
}
"#,
    );
    scenario.write_file(
        "data.crn",
        r#"let lookup = read mock.iam.Roles {
  name_regex = bucket.generated_rules[0].action
}
"#,
    );
    fs::write(&scenario.operation_log, "").expect("clear operation log");

    let plan = scenario.carina(&["plan", "."]);
    assert_success("plan after binding-preserving type change", &plan);
    let operations = fs::read_to_string(&scenario.operation_log).unwrap_or_default();
    assert!(
        !operations.contains("read iam.Roles.lookup"),
        "the configured runtime-dependent bucket must win over the stale orphan binding\n\
         operations:\n{operations}\noutput:\n{}",
        output_text(&plan),
    );
}
