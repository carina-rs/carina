use std::fs;
use std::path::PathBuf;
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
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(
            project.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write backend configuration");
        fs::write(
            project.join("resources.crn"),
            r#"let target = mock.test.resource { name = "target" }

let orphan = mock.test.resource {
  name   = "orphan"
  parent = target.name
}
"#,
        )
        .expect("write dependent resources");
        Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .args(args)
            .output()
            .expect("run carina")
    }

    fn remove_dependency_from_state_and_configuration(&self) {
        fs::write(self.project.join("resources.crn"), "")
            .expect("remove both resources from configuration");
        let state_path = self.project.join("carina.state.json");
        let mut state: Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read current state"))
                .expect("parse current state");
        state["resources"]
            .as_array_mut()
            .expect("state resources")
            .retain(|row| row["identity"] != "target");
        fs::write(
            state_path,
            serde_json::to_vec_pretty(&state).expect("encode edited state"),
        )
        .expect("write edited state");
    }

    fn prepare_v20_orphan_that_refreshes_absent_with_current_and_deposed_warnings(&self) {
        fs::write(self.project.join("resources.crn"), "")
            .expect("remove resources from configuration");

        let state_path = self.project.join("carina.state.json");
        let mut state: Value =
            serde_json::from_slice(&fs::read(&state_path).expect("read current state"))
                .expect("parse current state");
        state["version"] = Value::from(20);
        let resources = state["resources"]
            .as_array_mut()
            .expect("state resources");
        resources.retain(|row| row["identity"] == "orphan");
        let orphan = resources.first_mut().expect("orphan state row");
        orphan["dependency_bindings"] = serde_json::json!(["gone-current"]);
        orphan["deposed"] = serde_json::json!([{
            "key": "old-generation",
            "identifier": "orphan-old",
            "attributes": {},
            "dependency_bindings": ["gone-deposed"]
        }]);
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&state).expect("encode v20 orphan state"),
        )
        .expect("write v20 orphan state");

        let mut provider: Value =
            serde_json::from_slice(&fs::read(&self.mock_state).expect("read mock provider state"))
                .expect("parse mock provider state");
        provider["states"] = serde_json::json!({});
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&provider).expect("encode empty mock provider state"),
        )
        .expect("make orphan absent from provider");
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
        "{label} failed:\n{}",
        output_text(output)
    );
}

fn assert_typed_warning(output: &Output) {
    let text = output_text(output);
    assert!(
        text.contains("destroy ordering for mock.test.resource.orphan is partially unknown")
            && text.contains("persisted dependency `mock.test.resource.target` no longer resolves"),
        "missing typed persisted-dependency warning:\n{text}"
    );
}

#[test]
fn plan_and_apply_warn_but_delete_when_persisted_dependency_left_state_and_config() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "seed apply",
        &scenario.run(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.remove_dependency_from_state_and_configuration();

    let plan = scenario.run(&["plan", "."]);
    assert_success("plan orphan delete", &plan);
    assert_typed_warning(&plan);

    let apply = scenario.run(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("apply orphan delete", &apply);
    assert_typed_warning(&apply);
}

fn assert_legacy_current_and_deposed_warnings(output: &Output) {
    let text = output_text(output);
    assert!(
        text.contains("destroy ordering for mock.test.resource.orphan is partially unknown")
            && text.contains("gone-current")
            && text.contains("gone-deposed"),
        "migration must emit every row-scoped warning even without a Delete effect:\n{text}",
    );
}

#[test]
fn live_apply_emits_legacy_warnings_before_export_only_migration_write() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "seed apply",
        &scenario.run(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.prepare_v20_orphan_that_refreshes_absent_with_current_and_deposed_warnings();

    let apply = scenario.run(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("export-only migration apply", &apply);
    assert_legacy_current_and_deposed_warnings(&apply);

    let state: Value = serde_json::from_slice(
        &fs::read(scenario.project.join("carina.state.json")).expect("read migrated state"),
    )
    .expect("parse migrated state");
    assert_eq!(state["version"], Value::from(21));
}

#[test]
fn destroy_emits_legacy_warnings_when_refresh_filters_every_delete() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "seed apply",
        &scenario.run(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.prepare_v20_orphan_that_refreshes_absent_with_current_and_deposed_warnings();

    let destroy = scenario.run(&["destroy", "--auto-approve", "--lock=false", "."]);
    assert_success("empty destroy after migration", &destroy);
    assert_legacy_current_and_deposed_warnings(&destroy);
}
