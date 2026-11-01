//! End-to-end coverage for resource-handle module outputs (#3803).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_state::StateFile;
use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    producer_resources: PathBuf,
    state: PathBuf,
    provider_state: PathBuf,
    operation_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("module-handle fixture tempdir");
        let project = temp.path().to_path_buf();
        let producer = project.join("producer");
        let consumer = project.join("consumer");
        fs::create_dir(&producer).expect("create producer module");
        fs::create_dir(&consumer).expect("create consumer module");

        fs::write(
            project.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
provider mock {}

let producer_module = use { source = "./producer" }
let consumer_module = use { source = "./consumer" }

let producer = producer_module {}
let consumer = consumer_module {
  validation_record = producer.validation_record
}

let reader = mock.test.resource {
  name    = "reader"
  comment = consumer.observed_comment
}
"#,
        )
        .expect("write root fixture");
        let producer_resources = producer.join("resources.crn");
        fs::write(&producer_resources, producer_source("a-v1", "b-v1"))
            .expect("write producer resources");
        fs::write(
            producer.join("attributes.crn"),
            "attributes {\n  validation_record = validation_record\n}\n",
        )
        .expect("write producer handle output");
        fs::write(
            consumer.join("arguments.crn"),
            "arguments {\n  validation_record: mock.test.resource\n}\n",
        )
        .expect("write consumer argument");
        fs::write(
            consumer.join("resource.crn"),
            r#"let target = mock.test.resource {
  name    = "consumer"
  comment = validation_record.comment
  directives { depends_on = [validation_record] }
}
"#,
        )
        .expect("write consumer resource");
        fs::write(
            consumer.join("wait.crn"),
            r#"let ready = wait target {
  until      = target.name == "consumer"
  depends_on = [validation_record]
}
"#,
        )
        .expect("write consumer wait");
        fs::write(
            consumer.join("attributes.crn"),
            "attributes {\n  observed_comment: String = ready.comment\n}\n",
        )
        .expect("write consumer value output");

        Self {
            state: project.join("carina.state.json"),
            provider_state: project.join("mock-provider-state.json"),
            operation_log: project.join("operations.log"),
            producer_resources,
            project,
            _temp: temp,
        }
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.provider_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn write_producer(&self, a_comment: &str, b_comment: &str) {
        fs::write(
            &self.producer_resources,
            producer_source(a_comment, b_comment),
        )
        .expect("rewrite producer resources");
    }

    fn clear_operations(&self) {
        match fs::remove_file(&self.operation_log) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove operation log: {error}"),
        }
    }

    fn operations(&self) -> Vec<String> {
        fs::read_to_string(&self.operation_log)
            .expect("read operation log")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn json(&self, file: impl AsRef<Path>) -> Value {
        serde_json::from_str(
            &fs::read_to_string(self.project.join(file)).expect("read JSON fixture output"),
        )
        .expect("parse JSON fixture output")
    }
}

fn producer_source(a_comment: &str, b_comment: &str) -> String {
    format!(
        r#"let validation_record = mock.test.resource {{
  name    = "producer-a"
  comment = "{a_comment}"
}}

let unrelated = mock.test.resource {{
  name    = "producer-b"
  comment = "{b_comment}"
}}
"#,
    )
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
        String::from_utf8_lossy(&output.stderr),
    )
}

fn operation_position(operations: &[String], expected: &str) -> usize {
    operations
        .iter()
        .position(|operation| operation == expected)
        .unwrap_or_else(|| panic!("missing operation {expected:?}: {operations:#?}"))
}

fn assert_resolved_id(value: &Value, expected_identity: &str) {
    assert_eq!(value["provider"], "mock", "resolved id: {value:#}");
    assert_eq!(
        value["resource_type"], "test.resource",
        "resolved id: {value:#}",
    );
    assert_eq!(
        value["identity"], expected_identity,
        "resolved id: {value:#}",
    );
}

fn seed_v20_handle_dependencies(scenario: &Scenario) {
    // Use an output alias distinct from its internal binding: otherwise a
    // second graph build can accidentally resolve the spelling as a resource.
    let root = scenario.project.join("main.crn");
    let source = fs::read_to_string(&root).unwrap();
    fs::write(
        root,
        source
            .replace("producer.validation_record", "producer.r")
            .replace(
                "comment = consumer.observed_comment",
                "comment = consumer.observed_comment\n  directives { depends_on = [producer.r] }",
            ),
    )
    .unwrap();
    fs::write(
        scenario.project.join("producer/attributes.crn"),
        "attributes {\n  r = validation_record\n}\n",
    )
    .unwrap();
    let consumer = scenario.project.join("consumer/resource.crn");
    fs::write(
        &consumer,
        fs::read_to_string(&consumer).unwrap().replace(
            "comment = validation_record.comment",
            "comment = \"consumer\"",
        ),
    )
    .unwrap();
    assert_success("init handle migration", &scenario.command(&["init", "."]));
    assert_success(
        "seed handle migration",
        &scenario.command(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let mut state = scenario.json(&scenario.state);
    state["version"] = Value::from(20);
    for row in state["resources"].as_array_mut().unwrap() {
        let names = row["dependency_bindings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|dependency| dependency["identity"].clone())
            .collect::<Vec<_>>();
        row["dependency_bindings"] = Value::from(names);
        if let Some(dependencies) = row
            .get_mut("directives")
            .and_then(|directives| directives.get_mut("depends_on"))
            .and_then(Value::as_array_mut)
        {
            for dependency in dependencies {
                *dependency = dependency["identity"].clone();
            }
        }
    }
    fs::write(&scenario.state, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

#[test]
fn v20_plan_resolves_module_handle_outputs_and_p3_arguments_once() {
    let scenario = Scenario::new();
    seed_v20_handle_dependencies(&scenario);
    let before = fs::read(&scenario.state).unwrap();
    assert_success("plan v20 handles", &scenario.command(&["plan", "."]));
    assert_eq!(fs::read(&scenario.state).unwrap(), before);
}

#[test]
fn v20_apply_resolves_module_handle_outputs_and_p3_arguments_once() {
    let scenario = Scenario::new();
    seed_v20_handle_dependencies(&scenario);
    assert_success(
        "apply v20 handles",
        &scenario.command(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let state = scenario.json(&scenario.state);
    assert_eq!(state["version"], 21);
    let reader = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "reader")
        .unwrap();
    assert!(
        reader["dependency_bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|dependency| dependency["identity"] == "producer.validation_record")
    );
}

#[test]
fn v20_destroy_resolves_module_handle_outputs_and_p3_arguments_once() {
    let scenario = Scenario::new();
    seed_v20_handle_dependencies(&scenario);
    assert_success(
        "destroy v20 handles",
        &scenario.command(&["destroy", "--auto-approve", "--lock=false", "."]),
    );
}

#[test]
fn v20_state_refresh_resolves_module_handle_outputs_and_p3_arguments_once() {
    let scenario = Scenario::new();
    seed_v20_handle_dependencies(&scenario);
    assert_success(
        "refresh v20 handles",
        &scenario.command(&["state", "refresh", "--lock=false", "."]),
    );
}

#[test]
fn v20_migration_check_resolves_module_handle_outputs_and_p3_arguments_once() {
    let scenario = Scenario::new();
    seed_v20_handle_dependencies(&scenario);
    let before = fs::read(&scenario.state).unwrap();
    scenario.clear_operations();
    assert_success(
        "check v20 handles",
        &scenario.command(&[
            "state",
            "migrate",
            "--check",
            ".",
            "--state-url",
            "carina.state.json",
        ]),
    );
    assert!(fs::read(&scenario.state).unwrap() == before);
    assert!(
        !scenario.operation_log.exists(),
        "offline check must not contact the provider"
    );
}

#[test]
fn saved_p3_handles_round_trip_without_tainting_unchanged_dependents() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.command(&["init", "."]));

    let initial_plan =
        scenario.command(&["plan", "--refresh=false", "--out", "initial-plan.json", "."]);
    assert_success("initial saved plan", &initial_plan);
    let initial_json = scenario.json("initial-plan.json");
    assert_eq!(initial_json["version"], PlanFile::CURRENT_VERSION);
    let effects = initial_json["plan"]["effects"]
        .as_array()
        .expect("saved plan effects");
    let wait = effects
        .iter()
        .find_map(|effect| effect.get("Wait"))
        .unwrap_or_else(|| {
            panic!(
                "saved consumer wait effect: {effects:#?}\ncompositions: {:#?}",
                initial_json["compositions"],
            )
        });
    let wait_dependencies = wait["explicit_dependencies"]
        .as_array()
        .expect("typed wait dependency array");
    assert_eq!(wait_dependencies.len(), 1, "saved wait: {wait:#}");
    assert_resolved_id(&wait_dependencies[0], "producer.validation_record");
    assert!(
        wait_dependencies
            .iter()
            .all(|dependency| dependency["identity"] != "consumer.validation_record"),
        "a module parameter spelling reached the saved scheduler graph: {wait:#}",
    );
    let producer_composition = initial_json["compositions"]
        .as_array()
        .expect("saved compositions")
        .iter()
        .find(|composition| composition["binding"] == "producer")
        .expect("saved producer composition");
    assert!(
        producer_composition["attributes"]
            .as_object()
            .is_some_and(|attributes| !attributes.contains_key("validation_record")),
        "a handle output must be consumed before composition serialization: {producer_composition:#}",
    );
    let initial_wire = initial_json.to_string();
    for forbidden in ["StructuralHandle", "UnexpandedResourceHandle", "\"Handle\""] {
        assert!(
            !initial_wire.contains(forbidden),
            "saved plan leaked parser/live handle marker {forbidden:?}",
        );
    }

    scenario.clear_operations();
    assert_success(
        "apply initial saved plan",
        &scenario.command(&[
            "apply",
            "--parallelism",
            "8",
            "--auto-approve",
            "--lock=false",
            "initial-plan.json",
        ]),
    );
    assert_eq!(
        scenario.json(&scenario.state)["version"],
        StateFile::CURRENT_VERSION,
    );
    for json in [
        scenario.json(&scenario.state),
        scenario.json(&scenario.provider_state),
    ] {
        let wire = json.to_string();
        for forbidden in ["StructuralHandle", "UnexpandedResourceHandle", "\"Handle\""] {
            assert!(
                !wire.contains(forbidden),
                "state/provider JSON leaked handle marker {forbidden:?}: {wire}",
            );
        }
    }

    // Only B changes. A's handle is unchanged, so neither the consumer, its
    // wait, nor the reader of that wait may become unknown or acquire an
    // update merely because the dependency exists.
    scenario.write_producer("a-v1", "b-v2");
    let b_only = scenario.command(&["plan", "--refresh=false", "--out", "b-only-plan.json", "."]);
    assert_success("B-only plan", &b_only);
    let b_only_text = output_text(&b_only);
    assert!(
        b_only_text.contains("producer.unrelated"),
        "the fixture must actually plan B's update:\n{b_only_text}",
    );
    let rendered_b_only_plan = b_only_text
        .split("Plan:")
        .next()
        .expect("rendered plan summary");
    for forbidden in [
        "mock.test.resource consumer.target",
        "mock.test.resource reader",
        "(known after apply)",
    ] {
        assert!(
            !rendered_b_only_plan.contains(forbidden),
            "an unchanged A handle tainted a dependent via {forbidden:?}:\n{b_only_text}",
        );
    }
    let b_only_json = scenario.json("b-only-plan.json");
    let b_only_wire = b_only_json["plan"]["effects"].to_string();
    for forbidden in ["consumer.target", "\"reader\"", "__wait"] {
        assert!(
            !b_only_wire.contains(forbidden),
            "B-only saved effects unexpectedly contain dependent {forbidden:?}",
        );
    }

    assert_success(
        "apply B-only plan",
        &scenario.command(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "b-only-plan.json",
        ]),
    );

    // Changing A flows through the handle's ordinary attribute access. The
    // consumer update must run after A; the reader must in turn run after the
    // consumer's wait.
    scenario.write_producer("a-v2", "b-v2");
    let a_change = scenario.command(&[
        "plan",
        "--refresh=false",
        "--out",
        "a-change-plan.json",
        ".",
    ]);
    assert_success("A-change plan", &a_change);
    let a_change_text = output_text(&a_change);
    for expected in ["producer.validation_record", "consumer.target", "reader"] {
        assert!(
            a_change_text.contains(expected),
            "positive ordering fixture did not plan {expected}:\n{a_change_text}",
        );
    }

    scenario.clear_operations();
    assert_success(
        "apply A-change saved plan",
        &scenario.command(&[
            "apply",
            "--parallelism",
            "8",
            "--auto-approve",
            "--lock=false",
            "a-change-plan.json",
        ]),
    );
    let operations = scenario.operations();
    let producer = operation_position(
        &operations,
        "update test.resource.producer.validation_record",
    );
    let consumer = operation_position(&operations, "update test.resource.consumer.target");
    let reader = operation_position(&operations, "update test.resource.reader");
    assert!(
        producer < consumer && consumer < reader,
        "expected producer A -> consumer -> wait reader ordering: {operations:#?}",
    );
}
