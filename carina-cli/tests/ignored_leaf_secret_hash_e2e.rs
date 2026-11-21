//! Ignored observed leaves retain previous secret hashes inside sent attributes.

use std::{fs, process::Command};

use carina_core::value::PERSISTED_SECRET_KEY;
use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    project: TempDir,
    marker: String,
    resource_type: &'static str,
}

impl Scenario {
    fn new(resource_type: &'static str, attributes: &str) -> Self {
        let scenario = Self {
            project: tempfile::tempdir().expect("project directory"),
            marker: "fixture-secret-4570".into(),
            resource_type,
        };
        fs::write(
            scenario.project.path().join("providers.crn"),
            "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
        )
        .unwrap();
        scenario.write(attributes);
        scenario
    }

    fn write(&self, attributes: &str) {
        fs::write(
            self.project.path().join("resources.crn"),
            format!(
                "let sink = mock.{} {{\n{attributes}\n}}\n",
                self.resource_type
            )
            .replace("__MARKER__", &self.marker),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.project.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.project.path().join("provider-state.json"),
            )
            .args(args)
            .output()
            .expect("run carina");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let masked =
            format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&self.marker, "<MASKED>");
        assert!(output.status.success(), "{args:?} failed:\n{masked}");
        assert!(
            !stdout.contains(&self.marker) && !stderr.contains(&self.marker),
            "{args:?} exposed protected material:\n{masked}"
        );
        masked
    }

    fn attributes(&self) -> Value {
        let text = fs::read_to_string(self.project.path().join("carina.state.json")).unwrap();
        assert!(
            !text.contains(&self.marker),
            "state must not contain ignored secret plaintext"
        );
        let state: Value = serde_json::from_str(&text).expect("valid state JSON");
        state["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["binding"] == "sink")
            .expect("sink state row")["current"]["attributes"]
            .clone()
    }
}

fn assert_ignored_leaf_protected(
    resource_type: &'static str,
    initial: &str,
    edited: &str,
    retained: &str,
    changing: &str,
) -> Value {
    let scenario = Scenario::new(resource_type, initial);
    scenario.run(&["init", "."]);
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    let before = scenario.attributes();
    let digest = before.pointer(retained).unwrap()[PERSISTED_SECRET_KEY]
        .as_str()
        .expect("initial ignored leaf must be a hash");

    scenario.write(edited);
    let plan = scenario.run(&["plan", "."]);
    assert!(
        plan.contains("Plan: 0 to add, 1 to change, 0 to destroy."),
        "a sibling change must produce exactly one Update:\n{plan}"
    );
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    let after = scenario.attributes();
    assert!(
        after.pointer(retained).unwrap()[PERSISTED_SECRET_KEY].as_str() == Some(digest),
        "ignored leaf must keep its previous secret hash"
    );
    assert!(
        after.pointer(changing).unwrap() == "after",
        "sent sibling must be persisted as plaintext"
    );
    for args in [vec!["plan", "."], vec!["plan", "--refresh=false", "."]] {
        let plan = scenario.run(&args);
        assert!(
            plan.contains("No changes.")
                || plan.contains("Plan: 0 to add, 0 to change, 0 to destroy."),
            "the ignored leaf must not produce another update:\n{plan}"
        );
    }
    after
}

#[test]
fn ignored_map_leaf_in_sent_attribute_keeps_previous_secret_hash() {
    assert_ignored_leaf_protected(
        "test.resource",
        "name = 'sink'\ntags = { retained = secret('__MARKER__') changing = 'before' }",
        "name = 'sink'\ntags = { retained = 'public' changing = 'after' }\n\
         directives { ignore_changes = [tags[\"retained\"]] }",
        "/tags/retained",
        "/tags/changing",
    );
}

#[test]
fn ignored_map_leaf_beside_desired_secret_keeps_previous_secret_hash() {
    let after = assert_ignored_leaf_protected(
        "test.resource",
        "name = 'sink'\ntags = { retained = secret('__MARKER__') released = secret('__MARKER__') changing = 'before' }",
        "name = 'sink'\ntags = { retained = 'public' other = secret('__MARKER__') released = 'public' changing = 'after' }\n\
         directives { ignore_changes = [tags[\"retained\"]] }",
        "/tags/retained",
        "/tags/changing",
    );
    assert!(
        after["tags"]["other"][PERSISTED_SECRET_KEY].is_string(),
        "desired secret sibling must remain protected"
    );
    assert!(
        after["tags"]["released"] == "public",
        "a public sent leaf must release its previous hash"
    );
}

#[test]
fn ignored_map_leaf_under_opaque_hash_beside_desired_secret_remains_writable() {
    let scenario = Scenario::new(
        "test.resource",
        "name = 'sink'\ntags = secret({ retained = '__MARKER__' changing = 'before' })",
    );
    scenario.run(&["init", "."]);
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    let before = scenario.attributes();
    assert!(
        before["tags"][PERSISTED_SECRET_KEY].is_string(),
        "the authored secret map must start as one opaque hash"
    );

    scenario.write(
        "name = 'sink'\ntags = { retained = 'public' other = secret('__MARKER__') changing = 'after' }\n\
         directives { ignore_changes = [tags[\"retained\"]] }",
    );
    let plan = scenario.run(&["plan", "."]);
    assert!(
        plan.contains("Plan: 0 to add, 1 to change, 0 to destroy."),
        "the sibling change must produce exactly one Update:\n{plan}"
    );
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    let after = scenario.attributes();
    assert!(
        after["tags"] == before["tags"],
        "writeback must retain the opaque hash when a descendant is already masked"
    );

    let provider: Value = serde_json::from_slice(
        &fs::read(scenario.project.path().join("provider-state.json")).unwrap(),
    )
    .unwrap();
    let provider_tags = &provider["states"]
        .as_object()
        .unwrap()
        .values()
        .find(|row| row["name"] == "sink")
        .expect("provider sink row")["tags"];
    assert!(
        provider_tags["changing"] == "after",
        "the provider must have accepted the sibling update"
    );
    assert!(
        provider_tags["retained"].as_str() == Some(scenario.marker.as_str()),
        "the accepted update must graft the ignored observed leaf"
    );
}

#[test]
fn ignored_nested_struct_leaf_in_sent_attribute_keeps_previous_secret_hash() {
    assert_ignored_leaf_protected(
        "budgets.Budget",
        "budget = {\n\
           budget_name = 'sink'\n\
           budget_type = secret('__MARKER__')\n\
           time_unit = 'before'\n\
         }",
        "budget = {\n\
           budget_name = 'sink'\n\
           budget_type = 'public'\n\
           time_unit = 'after'\n\
         }\n\
         directives { ignore_changes = [budget.budget_type] }",
        "/budget/budget_type",
        "/budget/time_unit",
    );
}

#[test]
fn ignored_list_subtree_without_alignment_keeps_whole_hash_on_next_plan() {
    let initial = "name = 'sink'\n\
        lifecycle_configuration = {\n\
          transitions = [{ storage_class = secret('__MARKER__') }, { storage_class = 'visible-member' }]\n\
          transition = { storage_class = 'before' }\n\
        }";
    let edited = "name = 'sink'\n\
        lifecycle_configuration = {\n\
          transitions = [{ storage_class = 'public' }, { storage_class = 'visible-member' }]\n\
          transition = { storage_class = 'after' }\n\
        }";
    let scenario = Scenario::new("test.resource", initial);
    scenario.run(&["init", "."]);
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    scenario.attributes();
    scenario.write(&format!(
        "{edited}\ndirectives {{ ignore_changes = [lifecycle_configuration.transitions] }}"
    ));
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    let attributes = scenario.attributes();
    let config = &attributes["lifecycle_configuration"];
    assert!(
        config["transitions"][PERSISTED_SECRET_KEY].is_string(),
        "an unanchored list must be hashed as a whole"
    );
    assert!(
        config["transition"]["storage_class"] == "after",
        "the sent sibling remains public"
    );

    let shown = scenario.run(&["state", "show", "."]);
    assert!(
        shown.contains("(secret)") && !shown.contains("visible-member"),
        "state show must mask every member inside the hashed list:\n{shown}"
    );
    scenario.write(edited);
    for refresh in [false, true] {
        let flag = format!("--refresh={refresh}");
        let plan = scenario.run(&["plan", &flag, "."]);
        assert!(
            plan.contains("storage_class: 'visible-member'") && plan.contains("1 to change"),
            "the new public list must remain visible after removing the ignore:\n{plan}"
        );
    }
}
