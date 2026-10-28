use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    op_log: PathBuf,
}

impl Scenario {
    fn new(resources: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        fs::write(
            project.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
        )
        .expect("write main.crn");
        fs::write(project.join("resources.crn"), resources).expect("write resources.crn");
        let scenario = Self {
            mock_state: project.join("mock-state.json"),
            op_log: project.join("op.log"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.op_log)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn write_resources(&self, resources: &str) {
        fs::write(self.project.join("resources.crn"), resources).expect("rewrite resources.crn");
    }

    fn plan(&self) -> Output {
        self.carina(&["plan", "--out", "plan.json", "."])
    }

    fn apply_plan(&self) -> Output {
        self.carina(&["apply", "--auto-approve", "--lock=false", "plan.json"])
    }

    fn seed_state_and_inject_removed_type(&self) {
        assert_success(
            "seed apply",
            &self.carina(&["apply", "--auto-approve", "--lock=false", "."]),
        );

        let state_path = self.project.join("carina.state.json");
        let mut state = read_json(&state_path);
        let mut gone = state["resources"][0].clone();
        gone["resource_type"] = Value::String("example.gone".to_string());
        gone["identity"] = Value::String("g".to_string());
        gone["binding"] = Value::Null;
        gone["current"]["identifier"] = Value::String("gone-id".to_string());
        gone["current"]["attributes"] = serde_json::json!({ "name": "g" });
        gone["deposed"] = Value::Array(Vec::new());
        state["resources"]
            .as_array_mut()
            .expect("state resources array")
            .push(gone);
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&state).expect("encode state"),
        )
        .expect("write injected state");

        let mut provider_state = read_json(&self.mock_state);
        let seed = provider_state["states"]["test.resource.seed"].clone();
        provider_state["states"]
            .as_object_mut()
            .expect("mock states object")
            .insert("example.gone.g".to_string(), seed);
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&provider_state).expect("encode mock state"),
        )
        .expect("write injected mock state");
        fs::write(&self.op_log, "").expect("clear operation log");
    }

    fn remove_provider_config_and_retire_injected_type(&self) {
        fs::write(
            self.project.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
"#,
        )
        .expect("remove explicit provider config");

        let state_path = self.project.join("carina.state.json");
        let mut state = read_json(&state_path);
        let resources = state["resources"]
            .as_array_mut()
            .expect("state resources array");
        resources.retain(|row| row["resource_type"] == "example.gone");
        let retired = resources
            .first_mut()
            .expect("injected removed type remains in state");
        retired["provider"] = Value::String("retired".to_string());
        retired["current"]["kind"] = Value::String("missing_identifier".to_string());
        retired["current"]
            .as_object_mut()
            .expect("current instance object")
            .remove("identifier");
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&state).expect("encode retired-provider state"),
        )
        .expect("write retired-provider state");
    }

    fn remove_mock_provider_resource(&self, key: &str) {
        let mut provider_state = read_json(&self.mock_state);
        let removed = provider_state["states"]
            .as_object_mut()
            .expect("mock states object")
            .remove(key);
        assert!(removed.is_some(), "mock provider state has no {key}");
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&provider_state).expect("encode mock state"),
        )
        .expect("write mock state after out-of-band removal");
    }

    fn saved_plan_json(&self) -> Value {
        read_json(&self.project.join("plan.json"))
    }

    fn state_resource_types(&self) -> Vec<String> {
        read_json(&self.project.join("carina.state.json"))["resources"]
            .as_array()
            .expect("state resources array")
            .iter()
            .map(|row| {
                row["resource_type"]
                    .as_str()
                    .expect("resource type")
                    .to_string()
            })
            .collect()
    }
}

const SEED_RESOURCE: &str = r#"let seed = mock.test.resource {
  name = "seed"
}
"#;

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read JSON file")).expect("parse JSON file")
}

fn saved_schema_entry_for_provider<'a>(
    plan: &'a Value,
    provider: &str,
    resource_type: &str,
) -> &'a Value {
    plan["schema_fingerprints"]["providers"][provider]["schemas"]
        .as_array()
        .expect("saved schema entries")
        .iter()
        .find(|entry| entry["resource_type"] == resource_type)
        .unwrap_or_else(|| panic!("missing saved fingerprint for {resource_type}"))
}

fn saved_schema_entry<'a>(plan: &'a Value, resource_type: &str) -> &'a Value {
    saved_schema_entry_for_provider(plan, "mock", resource_type)
}

fn assert_absent_saved_fingerprint(plan: &Value, resource_type: &str) {
    assert_eq!(
        saved_schema_entry(plan, resource_type)["fingerprint"],
        "absent"
    );
}

#[test]
fn orphan_of_removed_provider_type_round_trips_through_saved_plan() {
    let scenario = Scenario::new(SEED_RESOURCE);
    scenario.seed_state_and_inject_removed_type();

    let plan = scenario.plan();
    assert_success("plan --out with unknown orphan type", &plan);
    assert_absent_saved_fingerprint(&scenario.saved_plan_json(), "example.gone");

    assert_success("apply saved orphan plan", &scenario.apply_plan());
    assert_eq!(scenario.state_resource_types(), vec!["test.resource"]);
}

#[test]
fn removed_block_for_removed_provider_type_round_trips_through_saved_plan() {
    let scenario = Scenario::new(SEED_RESOURCE);
    scenario.seed_state_and_inject_removed_type();
    scenario.write_resources(&format!(
        "{SEED_RESOURCE}\nremoved {{\n  from = mock.example.gone 'g'\n}}\n"
    ));

    let plan = scenario.plan();
    assert_success("plan --out with removed unknown type", &plan);
    assert_absent_saved_fingerprint(&scenario.saved_plan_json(), "example.gone");

    assert_success("apply saved removed plan", &scenario.apply_plan());
    assert_eq!(scenario.state_resource_types(), vec!["test.resource"]);
    let provider_state = read_json(&scenario.mock_state);
    assert!(
        provider_state["states"].get("example.gone.g").is_some(),
        "removed must leave provider state untouched"
    );
}

#[test]
fn state_only_unloaded_provider_round_trips_without_an_artifact() {
    let scenario = Scenario::new(SEED_RESOURCE);
    scenario.seed_state_and_inject_removed_type();
    scenario.remove_provider_config_and_retire_injected_type();
    scenario.write_resources("removed {\n  from = retired.example.gone 'g'\n}\n");

    let plan = scenario.plan();
    assert_success("plan --out with unloaded retired provider", &plan);
    let saved = scenario.saved_plan_json();
    assert_eq!(saved["provider_configs"], serde_json::json!([]));
    assert_eq!(
        saved["provider_artifacts"]
            .as_object()
            .expect("provider artifact object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["mock"],
        "the providerless fallback must record only the built-in mock: {saved:#}"
    );
    assert_eq!(
        saved_schema_entry_for_provider(&saved, "retired", "example.gone")["fingerprint"],
        "absent"
    );

    assert_success("apply saved retired-provider plan", &scenario.apply_plan());
    assert!(scenario.state_resource_types().is_empty());
}

#[test]
fn saved_plan_records_resource_kind_for_emitted_wait() {
    let scenario = Scenario::new(
        r#"let target = mock.test.resource {
  name    = "target"
  comment = "pending"
}

let target_ready = wait target {
  until = target.comment == "ready"
}

let consumer = mock.test.resource {
  name    = "consumer"
  comment = target_ready.comment
}
"#,
    );

    let plan = scenario.plan();
    assert_success("plan --out with managed-resource wait", &plan);
    let plan = scenario.saved_plan_json();
    let entries = plan["schema_fingerprints"]["providers"]["mock"]["schemas"]
        .as_array()
        .expect("saved schema entries")
        .iter()
        .filter(|entry| entry["resource_type"] == "test.resource")
        .collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        1,
        "wait target must be recorded once: {entries:?}"
    );
    assert_eq!(entries[0]["kind"], "resource");
}

#[test]
fn no_op_saved_plan_accepts_unchanged_current_state_only_schema() {
    let scenario = Scenario::new(
        r#"let a = mock.test.resource { name = "a" }
let t = mock.example.thing {}
"#,
    );
    assert_success(
        "initial apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    scenario.write_resources(
        r#"let a = mock.test.resource { name = "a" }
"#,
    );
    scenario.remove_mock_provider_resource("example.thing.t");

    let plan = scenario.plan();
    assert_success("no-op plan --out", &plan);
    let stdout = String::from_utf8_lossy(&plan.stdout);
    assert!(
        stdout.contains("No changes. Infrastructure is up-to-date."),
        "expected a no-op plan, got:\n{stdout}"
    );
    let saved = scenario.saved_plan_json();
    assert_eq!(
        saved["plan"]["effects"].as_array().map(Vec::len),
        Some(0),
        "reviewer repro must have zero effects: {saved:#}"
    );
    assert!(
        saved["current_states"]
            .as_array()
            .expect("saved current states")
            .iter()
            .any(
                |entry| entry.pointer("/id/resource_type").and_then(Value::as_str)
                    == Some("example.thing")
            ),
        "out-of-band-gone resource must remain in the saved current-state snapshot: {saved:#}"
    );
    assert!(
        saved_schema_entry(&saved, "example.thing")["fingerprint"]["present"]
            .as_str()
            .is_some(),
        "current-state-only type must retain its present fingerprint: {saved:#}"
    );

    assert_success("apply no-op saved plan", &scenario.apply_plan());
}
