//! End-to-end regression coverage for carina#4118.
//!
//! The resource, provider, and backend declarations live in sibling files so
//! planning and apply exercise the real directory-scoped module path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value as JsonValue;
use tempfile::TempDir;

const ATTRIBUTE: &str = "target_group_attributes";

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
    request_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().join("project");
        fs::create_dir(&project).expect("create project directory");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/server_added_list_elements");
        for file in ["main.crn", "providers.crn", "backend.crn"] {
            fs::copy(fixture.join(file), project.join(file)).expect("copy fixture file");
        }
        let scenario = Self {
            mock_state: temp.path().join("mock-state.json"),
            operation_log: temp.path().join("operations.log"),
            request_log: temp.path().join("requests.jsonl"),
            _temp: temp,
            project,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .env("CARINA_MOCK_UPDATE_REQUEST_LOG", &self.request_log)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn plan(&self) -> String {
        let output = self.carina(&["plan", "."]);
        assert_success("carina plan", &output);
        output_text(&output)
    }

    fn apply(&self) -> String {
        let output = self.carina(&["apply", "--auto-approve", "."]);
        assert_success("carina apply", &output);
        output_text(&output)
    }

    fn write_items(&self, items: &[(&str, &str)]) {
        let blocks = items
            .iter()
            .map(|(key, value)| {
                format!(
                    "\n  target_group_attribute {{\n    key   = '{key}'\n    value = '{value}'\n  }}\n"
                )
            })
            .collect::<String>();
        fs::write(
            self.project.join("main.crn"),
            format!(
                "mock.test.keyed_partial_update_list {{\n  name        = 'target'\n  description = 'keyed partial update'\n{blocks}}}\n"
            ),
        )
        .expect("write main.crn");
    }

    fn clear_provider_logs(&self) {
        fs::write(&self.operation_log, "").expect("clear operation log");
        fs::write(&self.request_log, "").expect("clear request log");
    }

    fn operation_log(&self) -> String {
        fs::read_to_string(&self.operation_log).unwrap_or_default()
    }

    fn update_requests(&self) -> Vec<JsonValue> {
        fs::read_to_string(&self.request_log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid request log JSON"))
            .collect()
    }

    fn mock_items(&self) -> Vec<JsonValue> {
        let document: JsonValue =
            serde_json::from_str(&fs::read_to_string(&self.mock_state).expect("read mock state"))
                .expect("parse mock state");
        document["states"]
            .as_object()
            .expect("mock states")
            .values()
            .next()
            .expect("mock resource")
            .get(ATTRIBUTE)
            .and_then(JsonValue::as_array)
            .expect("mock keyed list")
            .clone()
    }

    fn mock_value(&self, key: &str) -> Option<String> {
        self.mock_items()
            .into_iter()
            .find(|item| item["key"] == key)
            .and_then(|item| item["value"].as_str().map(str::to_owned))
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

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_release(text: &str, key: &str, value: &str) {
    let expected = format!(
        "! {ATTRIBUTE}[\"{key}\"]: left as '{value}' (AWS does not revert omitted keys; write the default value to revert)"
    );
    assert!(text.contains(&expected), "missing release row:\n{text}");
    assert!(
        !text.contains("- {"),
        "a release must not render as a list-element deletion:\n{text}"
    );
}

#[test]
fn keyed_partial_update_release_is_state_only_and_converges() {
    let scenario = Scenario::new();
    scenario.write_items(&[("a", "1"), ("b", "2")]);
    scenario.apply();
    scenario.clear_provider_logs();

    scenario.write_items(&[("a", "1")]);
    let plan = scenario.plan();
    assert_release(&plan, "b", "2");
    assert!(
        plan.contains("Plan: 0 to add, 0 to change, 0 to destroy, 1 to release."),
        "release must have a separate summary count:\n{plan}"
    );

    let apply = scenario.apply();
    assert_release(&apply, "b", "2");
    assert!(
        !scenario
            .operation_log()
            .lines()
            .any(|line| line.starts_with("update ")),
        "release-only apply reached provider update:\n{}",
        scenario.operation_log()
    );
    assert!(
        scenario.update_requests().is_empty(),
        "release-only apply captured an update request"
    );
    assert_eq!(scenario.mock_value("b").as_deref(), Some("2"));

    let next = scenario.plan();
    assert!(
        next.contains("No changes. Infrastructure is up-to-date."),
        "released key must be hidden on the next plan:\n{next}"
    );
    assert_eq!(scenario.mock_value("b").as_deref(), Some("2"));
}

#[test]
fn keyed_partial_update_release_and_update_sends_only_remaining_keys() {
    let scenario = Scenario::new();
    scenario.write_items(&[("a", "1"), ("b", "2")]);
    scenario.apply();
    scenario.clear_provider_logs();

    scenario.write_items(&[("a", "10")]);
    let plan = scenario.plan();
    assert_release(&plan, "b", "2");
    assert!(
        plan.contains("'1' → '10'"),
        "missing real value update:\n{plan}"
    );
    assert!(
        plan.contains("Plan: 0 to add, 1 to change, 0 to destroy, 1 to release."),
        "mixed plan must count update and release separately:\n{plan}"
    );

    scenario.apply();
    let requests = scenario.update_requests();
    assert_eq!(
        requests.len(),
        1,
        "expected one provider update: {requests:#?}"
    );
    let sent = requests[0]["ops"]
        .as_array()
        .expect("captured patch operations")
        .iter()
        .find(|op| op["key"] == ATTRIBUTE)
        .and_then(|op| op["value"].as_array())
        .expect("keyed list value in update patch");
    assert_eq!(sent.len(), 1, "released key was sent: {sent:#?}");
    assert_eq!(sent[0]["key"], "a");
    assert_eq!(sent[0]["value"], "10");
    assert_eq!(scenario.mock_value("a").as_deref(), Some("10"));
    assert_eq!(scenario.mock_value("b").as_deref(), Some("2"));

    let next = scenario.plan();
    assert!(
        next.contains("No changes. Infrastructure is up-to-date."),
        "mixed update/release must converge:\n{next}"
    );
}
