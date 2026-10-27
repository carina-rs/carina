use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        Self {
            mock_state: project.join("mock-state.json"),
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

    fn carina(&self, args: &[&str]) -> Output {
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
