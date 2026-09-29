//! Saved-plan regression coverage for composition serialization (carina#3798).
//!
//! The fixture deliberately includes every call shape whose boundary metadata
//! must survive live expansion while remaining absent from the saved-plan wire
//! format: bound calls with and without outputs, an anonymous call, and a
//! nested call. A managed resource consumes a module output so apply-from-plan
//! must also use the deserialized compositions for reference resolution.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        let with_outputs = project.join("with_outputs");
        let without_outputs = project.join("without_outputs");
        let outer = project.join("outer");
        for directory in [&with_outputs, &without_outputs, &outer] {
            fs::create_dir(directory).expect("create module directory");
        }

        fs::write(
            project.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
provider mock {}

let with_outputs = use { source = "./with_outputs" }
let without_outputs = use { source = "./without_outputs" }
let outer = use { source = "./outer" }

let producer = mock.test.resource {
  name = "published"
}

let published = with_outputs {
  name = producer.identifier
}

let silent = without_outputs {
  name = "silent"
}

without_outputs {
  name = "anonymous"
}

let wrapper = outer {
  name = "nested"
}

let consumer = mock.test.resource {
  name    = "consumer"
  comment = published.identifier
}
"#,
        )
        .expect("write root configuration");
        fs::write(
            with_outputs.join("main.crn"),
            r#"arguments {
  name: String {
    validation {
      condition     = length(name) > 0
      error_message = "name must not be empty"
    }
  }
}

let item = mock.test.resource {
  name = name
}

attributes {
  identifier: String = item.identifier
}
"#,
        )
        .expect("write output module");
        fs::write(
            without_outputs.join("main.crn"),
            r#"arguments {
  name: String
}

let item = mock.test.resource {
  name = name
}
"#,
        )
        .expect("write output-less module");
        fs::write(
            outer.join("main.crn"),
            r#"arguments {
  name: String
}

let inner = use { source = "../without_outputs" }

let nested = inner {
  name = name
}
"#,
        )
        .expect("write nested-call module");

        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        let output = scenario.carina(&["init", "."]);
        assert_success("carina init", &output);
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn plan_path(&self) -> PathBuf {
        self.project.join("plan.json")
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

fn read_saved_plan(path: &Path) -> (Value, PlanFile) {
    let json: Value = serde_json::from_str(
        &fs::read_to_string(path).expect("read saved plan produced by real CLI binary"),
    )
    .expect("parse saved-plan JSON");
    let plan = serde_json::from_value(json.clone()).expect("deserialize saved PlanFile");
    (json, plan)
}

#[test]
fn saved_plan_round_trips_and_applies_every_composition_shape() {
    let scenario = Scenario::new();

    let plan = scenario.carina(&["plan", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);

    let (saved_json, saved_plan) = read_saved_plan(&scenario.plan_path());
    assert_eq!(
        saved_plan.compositions.len(),
        5,
        "every bound, output-less, anonymous, and nested call must be saved: {:#?}",
        saved_plan.compositions,
    );

    let bindings = saved_plan
        .compositions
        .iter()
        .filter_map(|composition| composition.binding.as_deref())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        bindings,
        BTreeSet::from(["published", "silent", "wrapper", "wrapper.nested"]),
        "saved compositions must retain all authored and nested bindings",
    );
    assert_eq!(
        saved_plan
            .compositions
            .iter()
            .filter(|composition| composition.binding.is_none())
            .count(),
        1,
        "the anonymous module call must be represented by one composition",
    );

    let published = saved_plan
        .compositions
        .iter()
        .find(|composition| composition.binding.as_deref() == Some("published"))
        .expect("bound call with outputs");
    assert!(
        published.signature.attributes.contains_key("identifier"),
        "the output-bearing composition must retain its module attribute",
    );
    assert_eq!(
        published.signature.pending_constraints.len(),
        1,
        "the reference-valued argument constraint must survive plan serialization",
    );
    assert_eq!(
        published.signature.pending_constraints[0].message(),
        "name must not be empty",
    );
    let silent = saved_plan
        .compositions
        .iter()
        .find(|composition| composition.binding.as_deref() == Some("silent"))
        .expect("bound call without outputs");
    assert!(
        silent.signature.attributes.is_empty(),
        "the output-less bound call must still serialize as a composition",
    );

    for composition in &saved_plan.compositions {
        assert!(
            composition.diagnostic_call().is_none() && composition.diagnostic_root_call().is_none(),
            "saved composition {} must deserialize with Deserialized provenance",
            composition.instance,
        );
        for (name, argument) in &composition.signature.arguments {
            assert!(
                argument.declared_type().is_none(),
                "saved composition {} argument {name} must omit its declared type",
                composition.instance,
            );
        }
        for (name, attribute) in &composition.signature.attributes {
            assert!(
                attribute.declared_type().is_none(),
                "saved composition {} attribute {name} must omit its declared type",
                composition.instance,
            );
        }
    }

    let reserialized = serde_json::to_value(&saved_plan).expect("reserialize saved PlanFile");
    assert_eq!(
        saved_json.get("compositions"),
        reserialized.get("compositions"),
        "deserialize -> serialize must preserve the compositions wire section exactly",
    );

    let apply = scenario.carina(&["apply", "--auto-approve", "plan.json"]);
    assert_success("carina apply saved plan", &apply);
    let apply_text = output_text(&apply);
    for diagnostic_marker in ["module call '", "type mismatch", "cannot assign"] {
        assert!(
            !apply_text.contains(diagnostic_marker),
            "saved-plan apply must not re-check a Deserialized composition without its serde-skipped declared types; found {diagnostic_marker:?} in:\n{apply_text}",
        );
    }

    let replan = scenario.carina(&["plan", "."]);
    assert_success("carina re-plan after saved-plan apply", &replan);
    let replan_text = output_text(&replan);
    assert!(
        replan_text.contains("No changes. Infrastructure is up-to-date."),
        "re-plan after saved-plan apply must show no resource changes:\n{replan_text}",
    );
}
