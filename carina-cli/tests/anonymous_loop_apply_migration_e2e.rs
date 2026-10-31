//! Apply-level coverage for automatic migration from legacy synthetic loop addresses.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const LEGACY_IDENTITY: &str = "_for0[0]";

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
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            operation_log: project.join("mock-operations.log"),
            project,
            _temp: temp,
        };
        fs::write(
            scenario.project.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write backend configuration");
        fs::write(scenario.project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(
            scenario.project.join("main.crn"),
            "let seed = mock.test.resource { name = 'stable' }\n",
        )
        .expect("write seed configuration");

        assert_success("carina init", &scenario.carina(&["init", "."]));
        assert_success(
            "seed apply",
            &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
        );
        scenario.rewrite_seed_as_legacy_loop_row();
        fs::write(
            scenario.project.join("main.crn"),
            "for name in ['stable'] {\n  mock.test.resource { name = name }\n}\n",
        )
        .expect("write unbound loop configuration");
        scenario.clear_operation_log();
        scenario
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

    fn rewrite_seed_as_legacy_loop_row(&self) {
        let state_path = self.project.join("carina.state.json");
        let mut state = read_json(&state_path);
        let resources = state["resources"]
            .as_array_mut()
            .expect("Carina state resources");
        let [resource] = resources.as_mut_slice() else {
            panic!("seed apply must create exactly one resource: {resources:#?}");
        };
        resource["identity"] = Value::String(LEGACY_IDENTITY.to_string());
        resource["binding"] = Value::String(LEGACY_IDENTITY.to_string());
        write_json(&state_path, &state);

        let mut provider_state = read_json(&self.mock_state);
        let states = provider_state["states"]
            .as_object_mut()
            .expect("mock provider states");
        assert_eq!(
            states.len(),
            1,
            "seed apply must create exactly one mock provider row: {states:#?}"
        );
        let old_key = states.keys().next().expect("one provider row").clone();
        let attributes = states
            .remove(&old_key)
            .expect("seed mock provider row remains present");
        states.insert(format!("test.resource.{LEGACY_IDENTITY}"), attributes);
        write_json(&self.mock_state, &provider_state);
    }

    fn clear_operation_log(&self) {
        fs::write(&self.operation_log, "").expect("clear mock operation log");
    }

    fn operations(&self) -> String {
        fs::read_to_string(&self.operation_log).unwrap_or_default()
    }

    fn assert_migrated_without_provider_mutation(&self) {
        let state = read_json(&self.project.join("carina.state.json"));
        let resources = state["resources"]
            .as_array()
            .expect("Carina state resources");
        let [resource] = resources.as_slice() else {
            panic!("migration must retain exactly one state row: {resources:#?}");
        };
        let identity = resource["identity"]
            .as_str()
            .expect("migrated state identity");
        assert_ne!(identity, LEGACY_IDENTITY);
        assert!(
            identity.starts_with("mock_test_resource_"),
            "unexpected anonymous identity: {identity}"
        );
        assert!(
            resource.get("binding").is_none_or(Value::is_null),
            "anonymous state row must not retain the synthetic binding: {resource:#?}"
        );

        let operations = self.operations();
        assert!(
            operations
                .lines()
                .all(|line| !line.starts_with("create ") && !line.starts_with("delete ")),
            "Move migration must not call provider create/delete:\n{operations}"
        );
    }
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

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn live_apply_moves_legacy_loop_row_without_provider_mutation() {
    let scenario = Scenario::new();

    let apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);

    assert_success("live apply", &apply);
    scenario.assert_migrated_without_provider_mutation();
}

#[test]
fn current_saved_plan_apply_moves_legacy_loop_row_without_provider_mutation() {
    let scenario = Scenario::new();
    let plan = scenario.carina(&["plan", "--out", "migration.plan.json", "."]);
    assert_success("carina plan --out", &plan);

    let saved_plan = read_json(&scenario.project.join("migration.plan.json"));
    assert_eq!(saved_plan["version"], Value::from(32));
    let move_payload = saved_plan["plan"]["effects"]
        .as_array()
        .expect("saved plan effects")
        .iter()
        .find_map(|effect| effect.get("Move"))
        .expect("saved plan must contain the detected legacy Move");
    assert!(
        move_payload["origin"].is_object(),
        "the current saved-plan format must preserve the loop origin: {move_payload:#?}"
    );

    scenario.clear_operation_log();
    let apply = scenario.carina(&[
        "apply",
        "--auto-approve",
        "--lock=false",
        "migration.plan.json",
    ]);

    assert_success("saved-plan apply", &apply);
    scenario.assert_migrated_without_provider_mutation();
}
