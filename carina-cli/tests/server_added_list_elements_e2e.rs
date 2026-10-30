//! End-to-end regression coverage for carina-provider-awscc#511.
//!
//! The fixture deliberately splits the resource, provider, and backend across
//! sibling files. The mock resource models Cloud Control list attributes whose
//! read shape contains service-added `{ key, value }` elements.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_core::explicit::ExplicitFields;
use carina_state::StateFile;
use serde_json::Value as JsonValue;
use tempfile::TempDir;

const AUTHORED_KEY: &str = "deregistration_delay.timeout_seconds";
const DEFAULT_KEY: &str = "stickiness.type";
const SECOND_DEFAULT_KEY: &str = "load_balancing.algorithm.type";
const REMOVED_AUTHORED_KEY: &str = "proxy_protocol_v2.enabled";
const OUT_OF_BAND_KEY: &str = "out.of.band";
const INITIAL_DESCRIPTION: &str = "before";

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
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/server_added_list_elements");
        for file in ["main.crn", "providers.crn", "backend.crn"] {
            fs::copy(fixture.join(file), project.join(file)).expect("copy fixture file");
        }
        let mock_state = temp.path().join("mock-state.json");
        let scenario = Self {
            _temp: temp,
            project,
            mock_state,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
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

    fn apply(&self) {
        assert_success(
            "carina apply",
            &self.carina(&["apply", "--auto-approve", "."]),
        );
    }

    fn plan(&self) -> Output {
        let output = self.carina(&["plan", "."]);
        assert_success("carina plan", &output);
        output
    }

    fn save_plan(&self) {
        assert_success(
            "carina saved plan",
            &self.carina(&["plan", "--out", "plan.json", "."]),
        );
    }

    fn apply_saved_plan_with_out_of_band_addition(&self) {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env(
                "CARINA_MOCK_UPDATE_SERVER_LIST_OUT_OF_BAND_KEY",
                OUT_OF_BAND_KEY,
            )
            .env_remove("CLICOLOR_FORCE")
            .args(["apply", "--auto-approve", "plan.json"])
            .output()
            .expect("apply saved plan with provider-side addition");
        assert_success("carina apply saved plan", &output);
    }

    fn refresh(&self) {
        assert_success(
            "carina state refresh",
            &self.carina(&["state", "refresh", "--lock=false", "."]),
        );
    }

    fn write_authored_value(&self, value: &str) {
        self.write_authored_items(&[(AUTHORED_KEY, value)]);
    }

    fn write_authored_items(&self, items: &[(&str, &str)]) {
        self.write_configuration(INITIAL_DESCRIPTION, items);
    }

    fn write_description(&self, description: &str) {
        self.write_configuration(description, &[(AUTHORED_KEY, "60")]);
    }

    fn write_configuration(&self, description: &str, items: &[(&str, &str)]) {
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
                "mock.test.server_added_list {{\n  name        = 'target'\n  description = '{description}'\n{blocks}}}\n"
            ),
        )
        .expect("rewrite authored list value");
    }

    fn mutate_mock_list(&self, mutate: impl FnOnce(&mut Vec<JsonValue>)) {
        let mut document: JsonValue =
            serde_json::from_str(&fs::read_to_string(&self.mock_state).expect("read mock state"))
                .expect("parse mock state");
        let states = document["states"]
            .as_object_mut()
            .expect("mock state resources");
        let attributes = states
            .values_mut()
            .next()
            .expect("mock resource attributes")
            .as_object_mut()
            .expect("mock resource attribute map");
        let list = attributes["target_group_attributes"]
            .as_array_mut()
            .expect("mock target group attributes");
        mutate(list);
        fs::write(
            &self.mock_state,
            serde_json::to_string_pretty(&document).expect("serialize mock state"),
        )
        .expect("write mutated mock state");
    }

    fn state(&self) -> StateFile {
        carina_state::check_and_migrate(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read state"),
        )
        .expect("load state")
        .into_state()
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

fn assert_defaults_hidden(text: &str) {
    assert!(
        !text.contains(DEFAULT_KEY) && !text.contains(SECOND_DEFAULT_KEY),
        "server-added defaults must not be rendered:\n{text}"
    );
}

#[test]
fn server_added_list_elements_converge_after_apply_and_managed_refresh() {
    let scenario = Scenario::new();
    scenario.apply();

    let row = scenario
        .state()
        .resources()
        .first()
        .expect("applied state row")
        .clone();
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected resource Struct authoring record");
    };
    let ExplicitFields::ListElements { elements } = &children["target_group_attributes"] else {
        panic!("apply writeback must retain ListElements");
    };
    assert_eq!(elements.len(), 3);
    assert_eq!(
        elements
            .iter()
            .filter(|element| matches!(element, ExplicitFields::Unrecorded))
            .count(),
        2
    );

    scenario.refresh();
    let refreshed = scenario.state();
    let ExplicitFields::Struct { children } = &refreshed.resources()[0].explicit else {
        panic!("expected refreshed resource Struct authoring record");
    };
    assert!(matches!(
        children["target_group_attributes"],
        ExplicitFields::ListElements { .. }
    ));

    let text = output_text(&scenario.plan());
    assert!(
        text.contains("No changes. Infrastructure is up-to-date."),
        "unchanged configuration must converge after apply and refresh:\n{text}"
    );
}

#[test]
fn server_added_list_elements_show_only_authored_value_change() {
    let scenario = Scenario::new();
    scenario.apply();
    scenario.write_authored_value("30");

    let text = output_text(&scenario.plan());
    assert!(
        text.contains("'60' → '30'"),
        "missing authored value change:\n{text}"
    );
    assert_defaults_hidden(&text);
}

#[test]
fn server_added_list_elements_show_authored_removal() {
    let scenario = Scenario::new();
    scenario.apply();
    scenario.write_authored_items(&[(AUTHORED_KEY, "60"), (REMOVED_AUTHORED_KEY, "true")]);
    scenario.apply();
    scenario.write_authored_value("60");
    scenario.refresh();

    let refreshed = scenario.state();
    let ExplicitFields::Struct { children } = &refreshed.resources()[0].explicit else {
        panic!("expected refreshed resource Struct authoring record");
    };
    let ExplicitFields::ListElements { elements } = &children["target_group_attributes"] else {
        panic!("refresh must retain ListElements for the pending authored removal");
    };
    assert_eq!(
        elements
            .iter()
            .filter(|element| matches!(element, ExplicitFields::Unrecorded))
            .count(),
        2,
        "only the two provider defaults may be Unrecorded"
    );

    let text = output_text(&scenario.plan());
    assert!(
        text.contains(REMOVED_AUTHORED_KEY),
        "missing authored element removal:\n{text}"
    );
    assert_defaults_hidden(&text);
}

#[test]
fn server_added_list_elements_keep_unmatched_out_of_band_addition_visible() {
    let scenario = Scenario::new();
    scenario.apply();
    scenario.mutate_mock_list(|items| {
        items.push(serde_json::json!({
            "key": OUT_OF_BAND_KEY,
            "value": "enabled"
        }));
    });
    scenario.refresh();

    let refreshed = scenario.state();
    let row = &refreshed.resources()[0];
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected refreshed resource Struct authoring record");
    };
    let ExplicitFields::ListElements { elements } = &children["target_group_attributes"] else {
        panic!("refresh must retain element-aware list authoring");
    };
    let stored = row
        .persisted_attributes()
        .expect("refreshed attributes")
        .to_decoded_json();
    let items = stored["target_group_attributes"]
        .as_array()
        .expect("stored target group attributes");
    let out_of_band_index = items
        .iter()
        .position(|item| item["key"] == OUT_OF_BAND_KEY)
        .expect("stored out-of-band element");
    assert_eq!(
        elements[out_of_band_index],
        ExplicitFields::Leaf,
        "refresh must not classify an unsent out-of-band element as server-added"
    );

    let text = output_text(&scenario.plan());
    assert!(
        text.contains(OUT_OF_BAND_KEY),
        "unmatched out-of-band addition must remain visible:\n{text}"
    );
    assert_defaults_hidden(&text);
}

#[test]
fn server_added_list_elements_saved_plan_update_carries_unsent_list_authoring() {
    let scenario = Scenario::new();
    scenario.apply();
    scenario.write_description("after");
    scenario.save_plan();

    scenario.apply_saved_plan_with_out_of_band_addition();

    let refreshed = scenario.state();
    let row = &refreshed.resources()[0];
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected refreshed resource Struct authoring record");
    };
    let ExplicitFields::ListElements { elements } = &children["target_group_attributes"] else {
        panic!("saved-plan update must retain element-aware list authoring");
    };
    let stored = row
        .persisted_attributes()
        .expect("updated attributes")
        .to_decoded_json();
    let items = stored["target_group_attributes"]
        .as_array()
        .expect("stored target group attributes");
    let out_of_band_index = items
        .iter()
        .position(|item| item["key"] == OUT_OF_BAND_KEY)
        .expect("stored out-of-band element");
    assert_ne!(
        elements[out_of_band_index],
        ExplicitFields::Unrecorded,
        "an element read from an unsent attribute must not be classified as server-added\n\
         elements: {elements:#?}\nstored: {stored:#?}\nplan:\n{}",
        fs::read_to_string(scenario.project.join("plan.json")).expect("read saved plan")
    );

    let text = output_text(&scenario.plan());
    assert!(
        text.contains(OUT_OF_BAND_KEY),
        "the out-of-band element must remain visible after applying the saved plan:\n{text}"
    );
    assert_defaults_hidden(&text);
}

#[test]
fn server_added_list_elements_hide_uniquely_paired_default_value_drift() {
    let scenario = Scenario::new();
    scenario.apply();
    scenario.mutate_mock_list(|items| {
        let default = items
            .iter_mut()
            .find(|item| item["key"] == DEFAULT_KEY)
            .expect("server default element");
        default["value"] = JsonValue::String("app_cookie".to_string());
    });

    let text = output_text(&scenario.plan());
    assert!(
        text.contains("No changes. Infrastructure is up-to-date."),
        "a default element that still pairs uniquely by key stays hidden:\n{text}"
    );
}
