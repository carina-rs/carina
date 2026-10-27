//! Saved-plan secret verification for materialized deferred `for` children (#3990).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const SECRET_ENV: &str = "SAVED_PLAN_DEFERRED_FOR_SECRET_3990";
const PLANNED_SECRET: &str = "planned-deferred-for-secret-3990";
const CHANGED_SECRET: &str = "changed-deferred-for-secret-3990";

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/saved_plan_deferred_for_secrets");
        copy_fixture_tree(&fixture, &project);
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.run(&["init", "."], None));
        scenario
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .env_remove(SECRET_ENV)
            .args(args);
        command
    }

    fn run(&self, args: &[&str], secret: Option<&str>) -> Output {
        let mut command = self.command(args);
        if let Some(secret) = secret {
            command.env(SECRET_ENV, secret);
        }
        command.output().expect("run carina")
    }

    fn plan(&self) -> Output {
        self.run(&["plan", "--out", "plan.json", "."], Some(PLANNED_SECRET))
    }

    fn apply(&self, secret: Option<&str>) -> Output {
        self.run(
            &["apply", "--auto-approve", "--lock=false", "plan.json"],
            secret,
        )
    }

    fn apply_source(&self, secret: Option<&str>) -> Output {
        self.run(&["apply", "--auto-approve", "--lock=false", "."], secret)
    }

    fn write_project_file(&self, name: &str, contents: &str) {
        fs::write(self.project.join(name), contents).expect("write project fixture file");
    }

    fn inject_replacement_option_provider_default(&self, resource_name: &str) {
        let mut provider: Value = serde_json::from_str(
            &fs::read_to_string(&self.mock_state).expect("read mock provider state"),
        )
        .expect("parse mock provider state");
        let provider_resource = provider["states"]
            .as_object_mut()
            .expect("mock provider resources")
            .values_mut()
            .find(|resource| resource["name"] == resource_name)
            .unwrap_or_else(|| panic!("missing mock provider resource {resource_name:?}"));
        provider_resource["replacement_options"]["provider_default"] =
            Value::String("preserved".to_string());
        provider_resource["generated_rules"] = serde_json::json!([
            { "action": "moved-child" }
        ]);
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&provider).expect("serialize mock provider state"),
        )
        .expect("write mock provider state");

        let state_path = self.project.join("carina.state.json");
        let mut state: Value =
            serde_json::from_str(&fs::read_to_string(&state_path).expect("read Carina state"))
                .expect("parse Carina state");
        let state_resource = state["resources"]
            .as_array_mut()
            .expect("Carina state resources")
            .iter_mut()
            .find(|resource| resource["current"]["attributes"]["name"] == resource_name)
            .unwrap_or_else(|| panic!("missing Carina state resource {resource_name:?}"));
        state_resource["current"]["attributes"]["replacement_options"]["provider_default"] =
            Value::String("preserved".to_string());
        state_resource["current"]["attributes"]["generated_rules"] = serde_json::json!({
            "List": [{ "Map": { "action": { "String": "moved-child" } } }]
        });
        fs::write(
            state_path,
            serde_json::to_vec_pretty(&state).expect("serialize Carina state"),
        )
        .expect("write Carina state");
    }

    fn plan_text(&self) -> String {
        fs::read_to_string(self.project.join("plan.json")).expect("read saved plan")
    }

    fn state_text(&self) -> String {
        fs::read_to_string(self.project.join("carina.state.json")).expect("read state")
    }
}

fn copy_fixture_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create fixture destination");
    for entry in fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("read fixture file type").is_dir() {
            copy_fixture_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy fixture file");
        }
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
        output_text(output),
    );
}

fn assert_no_plaintext(label: &str, text: &str) {
    for plaintext in [PLANNED_SECRET, CHANGED_SECRET] {
        assert!(
            !text.contains(plaintext),
            "{label} leaked secret plaintext {plaintext:?}:\n{text}",
        );
    }
}

fn assert_plan_covers_every_expansion_shape(scenario: &Scenario) {
    let plan_text = scenario.plan_text();
    assert_no_plaintext("saved plan", &plan_text);
    let plan: Value = serde_json::from_str(&plan_text).expect("parse saved plan");
    let bindings = plan["sorted_resources"]
        .as_array()
        .expect("saved resource list")
        .iter()
        .filter_map(|resource| resource["binding"].as_str())
        .collect::<Vec<_>>();
    for expected in [
        "simple_children[0]",
        "indexed_children[0]",
        "mapped_children.alpha",
        "mapped_children.beta",
        "nested.module_children[0]",
    ] {
        assert!(
            bindings.contains(&expected),
            "saved plan did not materialize {expected:?}: {bindings:#?}",
        );
    }
}

fn assert_rejected_without_mutation(scenario: &Scenario, output: &Output, expected_message: &str) {
    let rendered = output_text(output);
    assert!(
        !output.status.success(),
        "saved-plan apply unexpectedly succeeded:\n{rendered}",
    );
    assert!(
        rendered.contains(expected_message) && rendered.contains("carina plan"),
        "saved-plan apply reported the wrong error:\n{rendered}",
    );
    assert_eq!(
        rendered.matches(expected_message).count(),
        1,
        "saved-plan apply rendered the verification cause more than once:\n{rendered}",
    );
    assert_no_plaintext("saved-plan apply output", &rendered);
    assert!(
        !scenario.mock_state.exists(),
        "provider was mutated before secret verification completed",
    );
    assert!(
        !scenario.project.join("carina.state.json").exists(),
        "state was created before secret verification completed",
    );
}

#[test]
fn unchanged_deferred_for_secrets_apply_and_stay_redacted() {
    let scenario = Scenario::new();
    let plan = scenario.plan();
    assert_success("carina plan --out", &plan);
    assert_no_plaintext("plan output", &output_text(&plan));
    assert_plan_covers_every_expansion_shape(&scenario);

    let apply = scenario.apply(Some(PLANNED_SECRET));
    assert_success("carina apply saved plan", &apply);
    assert_no_plaintext("apply output", &output_text(&apply));
    assert_no_plaintext("state", &scenario.state_text());

    let provider: Value = serde_json::from_str(
        &fs::read_to_string(&scenario.mock_state).expect("read mock provider state"),
    )
    .expect("parse mock provider state");
    let resources = provider
        .get("states")
        .unwrap_or(&provider)
        .as_object()
        .expect("mock provider resources");
    for expected_name in [
        "simple-child",
        "indexed-child-0",
        "mapped-child-a-alpha",
        "mapped-child-b-beta",
        "module-child",
    ] {
        let resource = resources
            .values()
            .find(|resource| resource["name"] == expected_name)
            .unwrap_or_else(|| panic!("missing applied resource {expected_name:?}: {provider:#}"));
        assert_eq!(resource["comment"], PLANNED_SECRET);
    }
}

#[test]
fn changed_deferred_for_secret_reports_hash_mismatch() {
    let scenario = Scenario::new();
    let plan = scenario.plan();
    assert_success("carina plan --out", &plan);
    assert_plan_covers_every_expansion_shape(&scenario);

    let apply = scenario.apply(Some(CHANGED_SECRET));
    assert_rejected_without_mutation(&scenario, &apply, "secret changed since the plan");
}

#[test]
fn unavailable_deferred_for_secret_reports_evaluation_failure() {
    let scenario = Scenario::new();
    let plan = scenario.plan();
    assert_success("carina plan --out", &plan);
    assert_plan_covers_every_expansion_shape(&scenario);

    let apply = scenario.apply(None);
    assert_rejected_without_mutation(
        &scenario,
        &apply,
        "current configuration could not be evaluated",
    );
    let rendered = output_text(&apply);
    assert!(
        !rendered.contains("secret changed since the plan"),
        "unavailable deferred-for secret must not be reported as a hash mismatch:\n{rendered}",
    );
}

#[test]
fn moved_iterable_source_uses_the_same_expansion_prelude_at_saved_apply() {
    let scenario = Scenario::new();
    scenario.write_project_file(
        "moved-source.crn",
        r#"let old_seed = mock.test.deferred_source {
  name = "moved-seed"
  replacement_options = {
    authored = "kept"
  }
}
"#,
    );
    let initial_apply = scenario.apply_source(Some(PLANNED_SECRET));
    assert_success("initial source apply", &initial_apply);
    assert_no_plaintext("initial source apply output", &output_text(&initial_apply));
    scenario.inject_replacement_option_provider_default("moved-seed");

    scenario.write_project_file(
        "moved-source.crn",
        r#"moved {
  from = mock.test.deferred_source 'old_seed'
  to   = mock.test.deferred_source 'renamed_seed'
}

let renamed_seed = mock.test.deferred_source {
  name = "moved-seed"
  replacement_options = {
    authored = "kept"
  }
}

let moved_children = for rule in renamed_seed.generated_rules {
  mock.test.resource {
    name    = rule.action
    comment = secret(env("SAVED_PLAN_DEFERRED_FOR_SECRET_3990"))
  }
}
"#,
    );
    let plan = scenario.plan();
    assert_success("moved-source saved plan", &plan);
    assert_no_plaintext("moved-source plan output", &output_text(&plan));
    let plan_text = scenario.plan_text();
    assert_no_plaintext("moved-source saved plan", &plan_text);
    let plan_json: Value = serde_json::from_str(&plan_text).expect("parse saved plan");
    assert!(
        plan_json["sorted_resources"]
            .as_array()
            .expect("saved resource list")
            .iter()
            .any(|resource| resource["binding"] == "moved_children[0]"),
        "plan-time expansion must materialize the child of the moved source",
    );

    let apply = scenario.apply(Some(PLANNED_SECRET));
    assert_success("moved-source saved apply", &apply);
    assert_no_plaintext("moved-source saved apply output", &output_text(&apply));
    assert_no_plaintext("moved-source state", &scenario.state_text());
}
