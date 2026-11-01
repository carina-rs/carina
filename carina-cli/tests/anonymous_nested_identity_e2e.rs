//! End-to-end coverage for nested anonymous identity and its address migration.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::{StateFile, check_and_migrate};
use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anonymous_nested_identity");
        for entry in fs::read_dir(fixture).expect("read nested-identity fixture") {
            let entry = entry.expect("read fixture entry");
            fs::copy(entry.path(), project.join(entry.file_name())).expect("copy fixture file");
        }
        Self {
            mock_state: project.join("mock-provider-state.json"),
            operation_log: project.join("mock-operations.log"),
            project,
            _temp: temp,
        }
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .args(args)
            .output()
            .expect("run carina")
    }

    fn state(&self) -> StateFile {
        check_and_migrate(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read local state"),
        )
        .expect("load local state")
        .into_state()
    }

    fn budget_identity(&self) -> String {
        let state = self.state();
        let rows = state
            .resources()
            .iter()
            .filter(|row| row.provider == "mock" && row.resource_type == "budgets.Budget")
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "expected one Budget row: {rows:#?}");
        rows[0].identity.as_str().to_string()
    }

    fn write_threshold(&self, threshold: f64) {
        let source = fs::read_to_string(self.project.join("budget.crn"))
            .expect("read Budget fixture source");
        let source = source.replace("threshold = 80.0", &format!("threshold = {threshold:.1}"));
        fs::write(self.project.join("budget.crn"), source).expect("edit Budget notification");
    }

    fn rewrite_budget_address(&self, old_identity: &str, new_identity: &str) {
        let state_path = self.project.join("carina.state.json");
        let mut state = read_json(&state_path);
        let resources = state["resources"]
            .as_array_mut()
            .expect("Carina state resources");
        let [resource] = resources.as_mut_slice() else {
            panic!("initial apply must create exactly one resource: {resources:#?}");
        };
        assert_eq!(
            resource["identity"],
            Value::String(new_identity.to_string())
        );
        resource["identity"] = Value::String(old_identity.to_string());
        write_json(&state_path, &state);

        let mut provider_state = read_json(&self.mock_state);
        let states = provider_state["states"]
            .as_object_mut()
            .expect("mock provider states");
        let new_key = format!("budgets.Budget.{new_identity}");
        let attributes = states
            .remove(&new_key)
            .unwrap_or_else(|| panic!("missing mock provider row {new_key}: {states:#?}"));
        states.insert(format!("budgets.Budget.{old_identity}"), attributes);
        write_json(&self.mock_state, &provider_state);
    }
}

fn old_rule_budget_identity() -> String {
    // Before #4162, only top-level create-only/identity attributes entered a
    // DefaultHasher. The Budget schema's mutable replacement list was the sole
    // top-level stable input; its natural key was nested and therefore absent.
    let mut old_inputs = BTreeMap::new();
    old_inputs.insert(
        "notifications_with_subscribers",
        concat!(
            "List([Map({\"notification_type\": String(\"ACTUAL\"), ",
            "\"threshold\": Float(80)})])",
        ),
    );
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in old_inputs {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    format!("mock_budgets_budget_{:08x}", hasher.finish() & 0xffff_ffff)
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
fn budget_notification_edit_replaces_at_the_same_nested_identity_address() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial carina apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let identity = scenario.budget_identity();

    scenario.write_threshold(90.0);
    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("replacement plan", &plan);
    let rendered = output_text(&plan);
    assert!(
        rendered.contains("Plan: 0 to add, 0 to change, 1 to replace, 0 to destroy."),
        "notification edit must be one replacement, not add+destroy:\n{rendered}",
    );
    assert!(
        rendered.contains(&format!(
            "-/+ mock.budgets.Budget {identity} (must be replaced)"
        )),
        "replacement must render at the nested-identity address {identity}:\n{rendered}",
    );
    assert_eq!(scenario.budget_identity(), identity);
}

#[test]
fn old_rule_budget_address_moves_once_then_converges() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "initial carina apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let new_identity = scenario.budget_identity();
    let old_identity = old_rule_budget_identity();
    assert_ne!(
        old_identity, new_identity,
        "fixture must exercise an address change"
    );
    scenario.rewrite_budget_address(&old_identity, &new_identity);

    let plan = scenario.carina(&[
        "plan",
        "--refresh=false",
        "--out",
        "identity-migration.plan.json",
        ".",
    ]);
    assert_success("identity migration plan", &plan);
    let saved = read_json(&scenario.project.join("identity-migration.plan.json"));
    let effects = saved["plan"]["effects"]
        .as_array()
        .expect("saved-plan effects");
    assert_eq!(
        effects.len(),
        1,
        "migration plan must contain only one effect: {effects:#?}"
    );
    let move_effect = effects[0]
        .get("Move")
        .unwrap_or_else(|| panic!("migration effect must be Move: {effects:#?}"));
    assert_eq!(move_effect["from"]["identity"], old_identity);
    assert_eq!(move_effect["to"]["identity"], new_identity);

    fs::write(&scenario.operation_log, "").expect("clear mock operation log");
    let apply = scenario.carina(&[
        "apply",
        "--auto-approve",
        "--lock=false",
        "identity-migration.plan.json",
    ]);
    assert_success("identity migration apply", &apply);
    assert_eq!(scenario.budget_identity(), new_identity);
    let operations = fs::read_to_string(&scenario.operation_log).unwrap_or_default();
    assert!(
        operations.lines().all(|line| {
            !line.starts_with("create ")
                && !line.starts_with("update ")
                && !line.starts_with("delete ")
        }),
        "Move must not mutate the provider object:\n{operations}",
    );

    let clean = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("post-migration plan", &clean);
    assert!(
        output_text(&clean).contains("No changes. Infrastructure is up-to-date."),
        "the next plan must be clean:\n{}",
        output_text(&clean),
    );
}
