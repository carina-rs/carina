use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            mock_state: project.join("mock-state.json"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self) {
        self.write_config_with_name("r1");
    }

    fn write_config_with_name(&self, name: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let r1 = mock.test.resource {{
  name = "{name}"
  tags = {{ version = "one" }}
}}
"#
            ),
        )
        .unwrap();
    }

    fn init(&self) {
        let output = carina(&self.project)
            .args(["init", "."])
            .output()
            .expect("failed to execute carina init");
        assert_success("carina init", &output);
    }

    fn apply_partial(&self) -> Output {
        carina(&self.project)
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_PARTIAL_CREATE_FOR", "*")
            .env("CARINA_MOCK_PARTIAL_CREATE_MISSING", "tags")
            .output()
            .expect("failed to execute carina apply")
    }

    fn plan(&self) -> Output {
        carina(&self.project)
            .args(["plan", "."])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("failed to execute carina plan")
    }
}

fn carina(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
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

fn contains_identifier(value: &Value, expected: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get("identifier").and_then(Value::as_str) == Some(expected)
                || map
                    .values()
                    .any(|value| contains_identifier(value, expected))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| contains_identifier(value, expected)),
        _ => false,
    }
}

#[test]
fn partial_create_records_state_and_next_plan_surfaces_unknown_reason() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();

    let apply = scenario.apply_partial();
    let apply_text = output_text(&apply);
    assert_eq!(
        apply.status.code(),
        Some(2),
        "partial apply must exit 2\nstdout/stderr:\n{apply_text}"
    );
    assert!(
        apply_text.contains("(partial)"),
        "apply output must render partial create\nstdout/stderr:\n{apply_text}"
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json")).unwrap();
    assert!(
        !state_text.contains("__carina_unknown"),
        "state file must not persist unknown sentinels:\n{state_text}"
    );
    assert!(
        state_text.contains("\"partial_read\""),
        "state file must persist the partial_read marker:\n{state_text}"
    );
    assert!(
        state_text.contains("\"missing_attributes\""),
        "state file must persist partial missing attributes:\n{state_text}"
    );
    let state: Value = serde_json::from_str(&state_text).unwrap();
    assert!(
        contains_identifier(&state, "mock-id"),
        "state file must contain the provider identifier:\n{}",
        serde_json::to_string_pretty(&state).unwrap()
    );

    let plan = scenario.plan();
    assert_success("carina plan", &plan);
    let plan_text = output_text(&plan);
    assert!(
        plan_text.contains("~ mock.test.resource r1") && plan_text.contains("1 to change"),
        "next plan must show the partially-created resource as an update\nstdout/stderr:\n{plan_text}"
    );
    assert!(
        plan_text
            .contains("(known after next apply: post-create read failed — mock partial create)"),
        "next plan must surface the partial-create unknown reason\nstdout/stderr:\n{plan_text}"
    );
}

#[test]
fn replace_partial_create_records_marker_for_next_plan() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();

    let first_apply = scenario.apply_partial();
    assert_eq!(
        first_apply.status.code(),
        Some(2),
        "initial partial apply must exit 2\nstdout/stderr:\n{}",
        output_text(&first_apply)
    );

    scenario.write_config_with_name("r1-replacement");
    let replace_apply = scenario.apply_partial();
    let replace_apply_text = output_text(&replace_apply);
    assert_eq!(
        replace_apply.status.code(),
        Some(2),
        "replace partial apply must exit 2\nstdout/stderr:\n{replace_apply_text}"
    );
    assert!(
        replace_apply_text.contains("(partial)"),
        "replace apply output must render partial create\nstdout/stderr:\n{replace_apply_text}"
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json")).unwrap();
    assert!(
        !state_text.contains("__carina_unknown"),
        "state file must not persist unknown sentinels:\n{state_text}"
    );
    assert!(
        state_text.contains("\"partial_read\""),
        "replace writeback must persist the partial_read marker:\n{state_text}"
    );

    let plan = scenario.plan();
    assert_success("carina plan after replace partial", &plan);
    let plan_text = output_text(&plan);
    assert!(
        plan_text.contains("~ mock.test.resource r1") && plan_text.contains("1 to change"),
        "next plan must show the replace partial resource as an update\nstdout/stderr:\n{plan_text}"
    );
    assert!(
        plan_text
            .contains("(known after next apply: post-create read failed — mock partial create)"),
        "next plan must surface the replace partial-create unknown reason\nstdout/stderr:\n{plan_text}"
    );
}
