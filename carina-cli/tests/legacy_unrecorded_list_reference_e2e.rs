//! End-to-end regression coverage for carina#4128.
//!
//! State written before carina#4124 could mark an authored list element as
//! `Unrecorded` when the DSL value was a still-unresolved resource reference.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value as JsonValue, json};
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().join("project");
        fs::create_dir(&project).expect("create project directory");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/legacy_unrecorded_list_reference");
        for file in ["main.crn", "providers.crn", "backend.crn"] {
            fs::copy(fixture.join(file), project.join(file)).expect("copy fixture file");
        }
        let scenario = Self {
            mock_state: temp.path().join("mock-state.json"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        assert_success("initial carina apply", &scenario.apply());
        scenario.seed_legacy_unrecorded_list_record();
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn apply(&self) -> Output {
        self.carina(&["apply", "--auto-approve", "."])
    }

    fn plan(&self) -> Output {
        let output = self.carina(&["plan", "."]);
        assert_success("carina plan", &output);
        output
    }

    fn refresh(&self) -> Output {
        self.carina(&["state", "refresh", "--lock=false", "."])
    }

    fn seed_legacy_unrecorded_list_record(&self) {
        let state_path = self.project.join("carina.state.json");
        let mut state: JsonValue =
            serde_json::from_slice(&fs::read(&state_path).expect("read applied state"))
                .expect("parse applied state");
        let consumer = state["resources"]
            .as_array_mut()
            .expect("state resources")
            .iter_mut()
            .find(|resource| resource["resource_type"] == "example.consumer")
            .expect("consumer state row");
        assert_eq!(
            consumer["current"]["attributes"]["list"],
            json!(["route-table-id"]),
            "the stored list must contain the resolved value that the authored reference produced"
        );
        consumer["explicit"]["children"]["list"] = json!({
            "kind": "list-elements",
            "elements": [{ "kind": "unrecorded" }]
        });
        fs::write(
            state_path,
            serde_json::to_vec_pretty(&state).expect("serialize legacy state"),
        )
        .expect("write legacy state");
    }

    fn write_unrelated_change(&self) {
        let path = self.project.join("main.crn");
        let source = fs::read_to_string(&path).expect("read main.crn");
        let updated = source.replace("plain = \"before\"", "plain = \"after\"");
        assert_ne!(source, updated, "fixture must contain the unrelated value");
        fs::write(path, updated).expect("write unrelated change");
    }

    fn write_route_table_replacement(&self) {
        let path = self.project.join("main.crn");
        let source = fs::read_to_string(&path).expect("read main.crn");
        let updated = source.replace(
            "name = \"route-table\"",
            "name = \"replacement-route-table\"",
        );
        assert_ne!(source, updated, "fixture must contain the route table name");
        fs::write(path, updated).expect("write route table replacement");
    }

    fn assert_consumer_list_element_is_recorded(&self, context: &str) {
        let state_path = self.project.join("carina.state.json");
        let state: JsonValue = serde_json::from_slice(&fs::read(&state_path).expect("read state"))
            .expect("parse state");
        let consumer = state["resources"]
            .as_array()
            .expect("state resources")
            .iter()
            .find(|resource| resource["resource_type"] == "example.consumer")
            .expect("consumer state row");
        let elements = consumer["explicit"]["children"]["list"]["elements"]
            .as_array()
            .expect("consumer list element records");
        assert_eq!(elements.len(), 1, "{context}: {consumer:#}");
        assert_ne!(
            elements[0],
            json!({ "kind": "unrecorded" }),
            "{context}: the desired-backed list element must no longer carry the stale legacy record: {consumer:#}"
        );
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
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_no_changes(output: &Output, context: &str) {
    let text = output_text(output);
    assert!(
        text.contains("No changes. Infrastructure is up-to-date."),
        "{context} must produce a clean plan:\n{text}"
    );
}

#[test]
fn desired_reference_keeps_legacy_unrecorded_list_element_authored() {
    let scenario = Scenario::new();

    assert_no_changes(
        &scenario.plan(),
        "an authored reference retained in desired state",
    );

    scenario.write_unrelated_change();
    assert_success("unrelated carina apply", &scenario.apply());
    assert_no_changes(
        &scenario.plan(),
        "the plan after applying an unrelated attribute change",
    );
    scenario.assert_consumer_list_element_is_recorded("after unrelated apply");

    assert_success("carina state refresh", &scenario.refresh());
    assert_no_changes(
        &scenario.plan(),
        "the plan after refreshing the repaired state row",
    );
    scenario.assert_consumer_list_element_is_recorded("after state refresh");
}

#[test]
fn unknown_replacement_reference_keeps_legacy_unrecorded_current_element_visible() {
    let scenario = Scenario::new();
    scenario.write_route_table_replacement();

    let text = output_text(&scenario.plan());
    let lines: Vec<_> = text.lines().collect();
    let list_line = lines
        .iter()
        .position(|line| line.trim_start().starts_with("list:"))
        .expect("consumer list diff must be rendered");
    let list_diff = lines[list_line..lines.len().min(list_line + 8)].join("\n");
    assert!(
        list_diff.contains("route-table-id"),
        "the unresolved replacement reference must not make projection drop the current authored element:\n{list_diff}\n\nfull plan:\n{text}"
    );
    assert!(
        list_diff.contains("route_table.identifier"),
        "the fixture must exercise an unresolved plan-time reference:\n{list_diff}\n\nfull plan:\n{text}"
    );
}
