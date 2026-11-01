//! Regression coverage for legacy identity migration after a permanent CBD name override.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create unique-name identity project");
        let project = temp.path().to_path_buf();
        fs::write(
            project.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write backend");
        fs::write(project.join("providers.crn"), "provider mock {}\n").expect("write provider");
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        scenario.write_resource("v1");
        scenario
    }

    fn write_resource(&self, replacement_key: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"mock.test.unique_identity {{
  name            = "role"
  replacement_key = "{replacement_key}"
  directives {{
    create_before_destroy = true
  }}
}}
"#,
            ),
        )
        .expect("write resource");
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .args(args)
            .output()
            .expect("run carina")
    }

    fn state_json(&self) -> Value {
        read_json(&self.project.join("carina.state.json"))
    }

    fn rewrite_address(&self, legacy_identity: &str, current_identity: &str) {
        let state_path = self.project.join("carina.state.json");
        let mut state = read_json(&state_path);
        let rows = state["resources"].as_array_mut().expect("state resources");
        let [row] = rows.as_mut_slice() else {
            panic!("expected one state row: {rows:#?}");
        };
        assert_eq!(row["identity"], current_identity);
        let persisted_attributes = row["current"]["attributes"].clone();
        row["identity"] = Value::String(legacy_identity.to_string());
        write_json(&state_path, &state);

        let mut provider = read_json(&self.mock_state);
        let states = provider["states"].as_object_mut().expect("mock states");
        let current_key = format!("test.unique_identity.{current_identity}");
        let attributes = states.remove(&current_key).unwrap_or_else(|| {
            assert!(
                states.is_empty(),
                "missing mock state {current_key}: {states:#?}"
            );
            persisted_attributes
        });
        states.insert(
            format!("test.unique_identity.{legacy_identity}"),
            attributes,
        );
        write_json(&self.mock_state, &provider);
    }
}

fn legacy_identity() -> String {
    let values = BTreeMap::from([
        ("name", "String(\"role\")"),
        ("replacement_key", "String(\"v2\")"),
    ]);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in values {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    format!(
        "mock_test_unique_identity_{:08x}",
        hasher.finish() & 0xffff_ffff
    )
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("serialize JSON fixture"),
    )
    .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
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
        "{label} failed\nstatus: {}\noutput:\n{}",
        output.status,
        output_text(output),
    );
}

#[test]
fn permanent_unique_name_override_still_matches_a_legacy_identity_row() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    scenario.write_resource("v2");
    assert_success(
        "CBD replacement apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    let state = scenario.state_json();
    let rows = state["resources"].as_array().expect("state resources");
    let [row] = rows.as_slice() else {
        panic!("expected one state row: {rows:#?}");
    };
    let current_identity = row["identity"].as_str().expect("current identity");
    let temporary_name = row
        .pointer("/current/attributes/name")
        .and_then(Value::as_str)
        .expect("persisted temporary name");
    assert_ne!(temporary_name, "role");
    assert_eq!(
        row.pointer("/name_overrides/name/temp_value")
            .and_then(Value::as_str),
        Some(temporary_name),
    );
    assert_eq!(
        row.pointer("/name_overrides/name/original_value")
            .and_then(Value::as_str),
        Some("role"),
    );

    let legacy_identity = legacy_identity();
    assert_ne!(legacy_identity, current_identity);
    scenario.rewrite_address(&legacy_identity, current_identity);

    let plan = scenario.carina(&[
        "plan",
        "--refresh=false",
        "--out",
        "identity-migration.plan.json",
        ".",
    ]);
    assert_success("legacy identity migration plan", &plan);
    let saved = read_json(&scenario.project.join("identity-migration.plan.json"));
    let effects = saved["plan"]["effects"]
        .as_array()
        .expect("saved plan effects");
    assert_eq!(
        effects.len(),
        1,
        "expected only the address move: {effects:#?}"
    );
    let move_effect = effects[0]
        .get("Move")
        .unwrap_or_else(|| panic!("expected a Move effect: {effects:#?}"));
    assert_eq!(move_effect["from"]["identity"], legacy_identity);
    assert_eq!(move_effect["to"]["identity"], current_identity);

    assert_success(
        "identity migration apply",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "identity-migration.plan.json",
        ]),
    );
    let clean = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("post-migration plan", &clean);
    assert!(
        output_text(&clean).contains("No changes. Infrastructure is up-to-date."),
        "the next plan must be clean:\n{}",
        output_text(&clean),
    );
}
