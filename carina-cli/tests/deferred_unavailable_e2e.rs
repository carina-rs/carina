//! End-to-end regressions for permissively failed builtin calls (#3974).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect as Effect;
use serde_json::Value;
use tempfile::TempDir;

const MISSING_ENVIRONMENT_VARIABLES: [&str; 7] = [
    "CARINA_MISSING_X",
    "CARINA_MISSING_A",
    "CARINA_MISSING_B",
    "SECRET_V",
    "PROBE_SECRET",
    "PROBE_PLAIN",
    "CARINA_MISSING_Y",
];

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn from_fixture(name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        for entry in fs::read_dir(&fixture).expect("read fixture directory") {
            let entry = entry.expect("read fixture entry");
            fs::copy(entry.path(), project.join(entry.file_name())).expect("copy fixture file");
        }
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args);
        for name in MISSING_ENVIRONMENT_VARIABLES {
            command.env_remove(name);
        }
        command
    }

    fn carina(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run carina")
    }

    fn carina_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        command.output().expect("run carina with environment")
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
        String::from_utf8_lossy(&output.stderr),
    )
}

fn provider_resources(provider: &Value) -> &serde_json::Map<String, Value> {
    provider
        .get("states")
        .unwrap_or(provider)
        .as_object()
        .expect("provider state object")
}

fn provider_resource_by_name<'a>(provider: &'a Value, name: &str) -> &'a Value {
    provider_resources(provider)
        .values()
        .find(|attributes| attributes.get("name") == Some(&Value::String(name.to_string())))
        .unwrap_or_else(|| panic!("provider state has no resource named {name}: {provider:#}"))
}

#[test]
fn saved_plan_preserves_an_unselected_unavailable_coalesce_fallback() {
    let scenario = Scenario::from_fixture("deferred_unavailable_saved_plan");

    let plan = scenario.carina(&["plan", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);
    let plan_output = format!(
        "{}{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr),
    );
    assert!(
        plan_output.contains("env('CARINA_MISSING_X')")
            && !plan_output.contains("env(<unavailable>)"),
        "plan display must preserve the unavailable call's arguments:\n{plan_output}",
    );
    let saved_plan =
        fs::read_to_string(scenario.project.join("plan.json")).expect("read saved plan JSON");
    assert!(
        saved_plan.contains("CARINA_MISSING_X"),
        "saved plan dropped the unselected env() fallback argument:\n{saved_plan}",
    );

    let apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "plan.json"]);
    assert_success("carina apply saved plan", &apply);

    let provider: Value = serde_json::from_str(
        &fs::read_to_string(&scenario.mock_state).expect("read mock provider state"),
    )
    .expect("parse mock provider state");
    assert_eq!(
        provider_resource_by_name(&provider, "p")["comment"],
        provider_resource_by_name(&provider, "o")["identifier"],
        "the selected reference must resolve without evaluating the missing env() fallback",
    );
}

#[test]
fn unavailable_env_arguments_keep_anonymous_resource_identities_distinct() {
    let scenario = Scenario::from_fixture("deferred_unavailable_identity");

    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);

    let saved_plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("plan.json")).expect("read saved plan"),
    )
    .expect("deserialize saved plan");
    let identities = saved_plan
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Create(resource) if resource.get_attr("replacement_key").is_some() => {
                resource.id.identity_str().map(str::to_string)
            }
            _ => None,
        })
        .collect::<HashSet<_>>();

    assert_eq!(
        identities.len(),
        2,
        "different env() arguments must produce distinct anonymous identities: {identities:?}",
    );
}

#[test]
fn secret_wrapped_unavailable_interpolations_load_but_fail_when_planned() {
    let scenario = Scenario::from_fixture("deferred_unavailable_secret_load");

    assert_success("carina validate", &scenario.carina(&["validate", "."]));

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let rendered = output_text(&plan);
    assert!(
        !plan.status.success(),
        "plan unexpectedly accepted a required missing secret input:\n{rendered}",
    );
    assert!(
        rendered.contains("env()")
            && rendered.contains("SECRET_V")
            && rendered.contains("is not set"),
        "plan lost the original env() diagnostic:\n{rendered}",
    );
}

#[test]
fn saved_plan_does_not_misattribute_an_unrelated_unavailable_map_entry() {
    let scenario = Scenario::from_fixture("deferred_unavailable_unrelated_map");

    let plan = scenario.carina_with_env(
        &["plan", "--out", "plan.json", "."],
        &[
            ("PROBE_SECRET", "secret-value"),
            ("PROBE_PLAIN", "plain-value"),
        ],
    );
    assert_success("carina plan --out", &plan);

    let apply = scenario.carina_with_env(
        &["apply", "--auto-approve", "--lock=false", "plan.json"],
        &[("PROBE_SECRET", "secret-value")],
    );
    assert_success("carina apply saved plan", &apply);
}

#[test]
fn saved_plan_ignores_an_unavailable_coalesce_default_outside_the_secret() {
    let scenario = Scenario::from_fixture("deferred_unavailable_unrelated_coalesce");

    let plan = scenario.carina(&["plan", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);

    let apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "plan.json"]);
    assert_success("carina apply saved plan", &apply);
}
