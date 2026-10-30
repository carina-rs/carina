//! End-to-end recovery coverage for a create whose cloud outcome is unknown.
//!
//! Both scenarios use a directory-scoped four-file project so recovery is
//! exercised through the same merged configuration path as a real project.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

struct OutcomeUnknownProject {
    _temp: TempDir,
    root: PathBuf,
    mock_state: PathBuf,
}

impl OutcomeUnknownProject {
    fn new(identity: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let root = temp.path().to_path_buf();
        fs::write(
            root.join("main.crn"),
            format!("let tracked = mock.test.resource {{\n  name = \"{identity}\"\n}}\n"),
        )
        .expect("write main.crn");
        fs::write(root.join("providers.crn"), "provider mock {}\n").expect("write providers.crn");
        fs::write(
            root.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend.crn");
        fs::write(
            root.join("exports.crn"),
            "exports {\n  tracked_name: String = tracked.name\n}\n",
        )
        .expect("write exports.crn");
        Self {
            mock_state: root.join("mock-provider-state.json"),
            root,
            _temp: temp,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(args)
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("run carina")
    }

    fn initialize(&self) {
        assert_success("carina init", &self.run(&["init", "."]));
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("carina.state.json")
    }

    fn read_state_json(&self) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.state_path()).expect("read local state file"))
            .expect("parse local state file")
    }

    fn write_state_json(&self, state: &serde_json::Value) {
        let mut encoded = serde_json::to_string_pretty(state).expect("encode local state file");
        encoded.push('\n');
        fs::write(self.state_path(), encoded).expect("write local state file");
    }

    fn append_import(&self, identity: &str, identifier: &str) {
        let main = fs::read_to_string(self.root.join("main.crn")).expect("read main.crn");
        fs::write(
            self.root.join("main.crn"),
            format!(
                "{main}\nimport {{ to = mock.test.resource \"{identity}\" id = \"{identifier}\" }}\n"
            ),
        )
        .expect("append import block");
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
        "{label} failed\n{}",
        output_text(output)
    );
}

fn replace_applied_current_with_unknown(state: &mut serde_json::Value) -> (String, String) {
    let row = state["resources"]
        .as_array_mut()
        .and_then(|rows| rows.first_mut())
        .expect("applied fixture has one resource row");
    let identity = row["identity"]
        .as_str()
        .expect("state row has identity")
        .to_string();
    let identifier = row["current"]["identifier"]
        .as_str()
        .expect("applied state row has provider identifier")
        .to_string();
    row["current"] = serde_json::json!({
        "kind": "create_outcome_unknown",
        "started_at": "2026-10-30T03:00:00Z"
    });
    (identity, identifier)
}

fn seed_unknown_state(project: &OutcomeUnknownProject, identity: &str) {
    project.write_state_json(&serde_json::json!({
        "version": carina_state::StateFile::CURRENT_VERSION,
        "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "create-outcome-unknown-e2e",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.resource",
            "identity": identity,
            "provider": "mock",
            "current": {
                "kind": "create_outcome_unknown",
                "started_at": "2026-10-30T03:00:00Z"
            },
            "deposed": [{
                "key": "00000000-0000-4000-8000-000000000001",
                "identifier": "previous-applied-id",
                "attributes": {},
                "dependency_bindings": []
            }]
        }],
        "exports": {}
    }));
}

fn assert_plan_is_blocked(project: &OutcomeUnknownProject, identity: &str) {
    let plan = project.run(&["plan", "."]);
    let text = output_text(&plan);
    assert!(
        !plan.status.success(),
        "outcome-unknown state must block plan\n{text}"
    );
    let expected = format!(
        "if it exists: import {{ to = mock.test.resource \"{identity}\" id = \"…\" }}; \
         if it doesn't: carina state forget mock.test.resource {identity}"
    );
    assert!(
        text.contains(&expected),
        "blocked plan must render the exact two recovery paths\nexpected: {expected}\n{text}"
    );
}

#[test]
fn unknown_row_blocks_then_import_replaces_it_and_apply_converges() {
    let project = OutcomeUnknownProject::new("outcome-unknown-import");
    project.initialize();
    assert_success(
        "initial apply",
        &project.run(&["apply", ".", "--auto-approve"]),
    );

    let mut state = project.read_state_json();
    let (identity, identifier) = replace_applied_current_with_unknown(&mut state);
    project.write_state_json(&state);

    assert_plan_is_blocked(&project, &identity);
    project.append_import(&identity, &identifier);

    assert_success("import plan", &project.run(&["plan", "."]));
    assert_success(
        "import apply",
        &project.run(&["apply", ".", "--auto-approve"]),
    );

    let imported = project.read_state_json();
    assert_eq!(imported["resources"][0]["current"]["kind"], "applied");
    assert_eq!(
        imported["resources"][0]["current"]["identifier"],
        identifier
    );
    let converged = project.run(&["plan", "."]);
    assert_success("converged plan", &converged);
    assert!(
        output_text(&converged).contains("No changes. Infrastructure is up-to-date."),
        "import recovery must converge on the next plan\n{}",
        output_text(&converged)
    );
}

#[test]
fn unknown_row_forget_allows_the_next_plan_to_propose_create() {
    let identity = "outcome-unknown-forget";
    let project = OutcomeUnknownProject::new(identity);
    project.initialize();
    seed_unknown_state(&project, identity);

    assert_plan_is_blocked(&project, identity);
    let forget = project.run(&[
        "state",
        "forget",
        "mock.test.resource",
        identity,
        "--auto-approve",
        ".",
    ]);
    assert_success("state forget", &forget);
    let expected_message =
        format!("Forgot create-outcome-unknown current instance: mock.test.resource '{identity}'");
    assert!(
        output_text(&forget).contains(&expected_message),
        "state forget must show the current instance it removed\n{}",
        output_text(&forget)
    );
    let forgotten = project.read_state_json();
    let resources = forgotten["resources"].as_array().expect("state resources");
    assert_eq!(resources.len(), 1, "the deposed-only shell must remain");
    assert_eq!(resources[0]["current"]["kind"], "none");
    assert_eq!(resources[0]["deposed"].as_array().unwrap().len(), 1);
    assert_eq!(
        resources[0]["deposed"][0]["identifier"],
        "previous-applied-id"
    );

    let plan = project.run(&["plan", "."]);
    assert_success("post-forget plan", &plan);
    let text = output_text(&plan);
    assert!(
        text.contains("Plan: 1 to add, 0 to change, 1 to destroy"),
        "post-forget plan must propose the ordinary create and retain the deposed delete\n{text}"
    );
}
