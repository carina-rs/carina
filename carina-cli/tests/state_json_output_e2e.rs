use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Project {
    _temp: TempDir,
    root: PathBuf,
    mock_state: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = TempDir::new().expect("temporary project");
        let root = temp.path().to_path_buf();
        fs::write(
            root.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write backend.crn");
        fs::write(root.join("providers.crn"), "provider mock {}\n").expect("write providers.crn");
        fs::write(
            root.join("main.crn"),
            "let r1 = mock.test.resource { name = 'r1' }\n",
        )
        .expect("write main.crn");
        Self {
            mock_state: root.join("mock-provider-state.json"),
            root,
            _temp: temp,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn apply(&self) {
        assert_success("init", &self.run(&["init", "."]));
        assert_success("apply", &self.run(&["apply", ".", "--auto-approve"]));
    }

    fn root(&self) -> &Path {
        &self.root
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

fn json_output(label: &str, output: &Output) -> Value {
    assert_success(label, output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{label} did not emit JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

#[test]
fn state_show_json_without_state_returns_absent_document() {
    let project = Project::new();
    assert!(project.root().join("backend.crn").is_file());
    assert!(project.root().join("providers.crn").is_file());
    assert!(project.root().join("main.crn").is_file());

    let output = project.run(&["state", "show", "--json", "."]);
    let document = json_output("state show --json without state", &output);

    assert_eq!(
        document,
        json!({"schema_version": 1, "state": "absent", "resources": []})
    );
}

#[test]
fn applied_state_show_and_lookup_share_the_versioned_json_contract() {
    let project = Project::new();
    project.apply();

    let shown = json_output(
        "state show --json",
        &project.run(&["state", "show", "--json", "."]),
    );
    assert_eq!(shown["schema_version"], 1);
    assert_eq!(shown["state"], "present");
    let row = shown["resources"]
        .as_array()
        .expect("resources must be an array")
        .iter()
        .find(|row| row["binding"] == "r1")
        .expect("applied resource must be present");
    assert!(row["current"]["identifier"].is_string());

    let lookup_row = json_output(
        "state lookup --json resource",
        &project.run(&["state", "lookup", "--json", "r1", "."]),
    );
    assert_eq!(&lookup_row, row);

    let lookup_attribute = json_output(
        "state lookup --json attribute",
        &project.run(&["state", "lookup", "--json", "r1.name", "."]),
    );
    assert_eq!(lookup_attribute, json!("r1"));
}
