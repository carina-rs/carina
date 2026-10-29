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

fn write_nested_secret_tree(checkout: &Path) -> PathBuf {
    let config = checkout.join("infra/env");
    let outer = checkout.join("modules/outer");
    let inner = checkout.join("modules/inner");
    for directory in [&config, &outer, &inner] {
        fs::create_dir_all(directory).expect("create copied-tree fixture directory");
    }
    fs::write(
        config.join("main.crn"),
        r#"backend local { path = "carina.state.json" }
provider mock {}

let outer = use { source = '../../modules/outer' }
let wrapper = outer { label = "public" }
"#,
    )
    .expect("write copied-tree root configuration");
    fs::write(
        outer.join("main.crn"),
        r#"arguments {
  label: String
}

let inner = use { source = '../inner' }

let nested = inner {
  value = secret("PORTABLE_NESTED_SECRET_4041")
}
"#,
    )
    .expect("write copied-tree outer module");
    fs::write(
        inner.join("main.crn"),
        r#"arguments {
  value: String
}

let item = mock.test.resource {
  name    = "portable-nested"
  comment = value
}
"#,
    )
    .expect("write copied-tree inner module");
    config
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copied checkout directory");
    for entry in fs::read_dir(source).expect("read source checkout") {
        let entry = entry.expect("read source checkout entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("read entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy checkout file");
        }
    }
}

fn run_in_checkout(checkout: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(checkout)
        .env("NO_COLOR", "1")
        .env(
            "CARINA_MOCK_STATE_FILE",
            checkout.join("mock-provider-state.json"),
        )
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina in copied-tree checkout")
}

fn saved_call_location(call: &Value) -> String {
    format!(
        "{}:{}:{}",
        call["source_file"]["path"]
            .as_str()
            .expect("saved call source path"),
        call["call_span"]["start_line"]
            .as_u64()
            .expect("saved call start line"),
        call["call_span"]["start_column"]
            .as_u64()
            .expect("saved call start column"),
    )
}

#[test]
fn saved_nested_secret_plan_with_parent_module_path_applies_after_tree_copy() {
    let source_temp = tempfile::tempdir().expect("source checkout tempdir");
    let source_checkout = source_temp.path().join("checkout");
    let source_config = write_nested_secret_tree(&source_checkout);
    assert_success(
        "carina init source checkout",
        &run_in_checkout(&source_checkout, &["init", "infra/env"]),
    );
    assert_success(
        "carina plan source checkout",
        &run_in_checkout(
            &source_checkout,
            &["plan", "--out", "infra/env/plan.json", "infra/env"],
        ),
    );

    let source_plan_path = source_config.join("plan.json");
    let source_plan_text = fs::read_to_string(&source_plan_path).expect("read source saved plan");
    let source_plan_json: Value =
        serde_json::from_str(&source_plan_text).expect("parse source saved plan");
    let source_modules = source_checkout
        .join("modules")
        .canonicalize()
        .expect("canonical source modules");
    assert!(
        !source_plan_text.contains(&source_modules.display().to_string()),
        "saved plan JSON leaked an absolute module path {}:\n{source_plan_text}",
        source_modules.display(),
    );
    for composition in source_plan_json["compositions"]
        .as_array()
        .expect("saved compositions")
    {
        let origin = &composition["origin_id"];
        let mut calls = vec![&origin["root"]];
        calls.extend(
            origin["descendants"]
                .as_array()
                .expect("saved origin descendants"),
        );
        for call in calls {
            let path = call["source_file"]["path"]
                .as_str()
                .expect("saved call source path");
            assert!(
                !Path::new(path).is_absolute(),
                "saved plan JSON contains an absolute composition source path {path:?}",
            );
        }
    }
    let nested = source_plan_json["compositions"]
        .as_array()
        .expect("saved compositions")
        .iter()
        .find(|composition| composition["binding"] == "wrapper.nested")
        .expect("saved nested composition");
    assert_eq!(
        nested["origin_id"]["descendants"][0]["source_file"]["path"],
        "../../modules/outer/main.crn",
        "the external module file must be normalized relative to the source root",
    );

    let destination_temp = tempfile::tempdir().expect("destination checkout tempdir");
    let destination_checkout = destination_temp.path().join("checkout");
    copy_tree(&source_checkout, &destination_checkout);
    let destination_config = destination_checkout.join("infra/env");
    let destination_plan_path = destination_config.join("plan.json");

    // Saved plans record the selected project separately from composition
    // identity. Rebind that existing field to the copied checkout so this
    // regression isolates whether nested call identities are portable.
    let mut copied_plan: Value = serde_json::from_str(
        &fs::read_to_string(&destination_plan_path).expect("read copied saved plan"),
    )
    .expect("parse copied saved plan");
    copied_plan["source_path"] = Value::String(destination_config.display().to_string());
    fs::write(
        &destination_plan_path,
        serde_json::to_vec_pretty(&copied_plan).expect("serialize rebound copied plan"),
    )
    .expect("write rebound copied plan");

    assert_success(
        "carina apply copied saved plan",
        &run_in_checkout(
            &destination_checkout,
            &["apply", "--auto-approve", "infra/env/plan.json"],
        ),
    );
}

#[test]
fn moved_root_call_error_lists_saved_root_to_immediate_chain() {
    let temp = tempfile::tempdir().expect("moved-call fixture tempdir");
    let checkout = temp.path().join("checkout");
    let config = write_nested_secret_tree(&checkout);
    assert_success(
        "carina init moved-call fixture",
        &run_in_checkout(&checkout, &["init", "infra/env"]),
    );
    assert_success(
        "carina plan moved-call fixture",
        &run_in_checkout(
            &checkout,
            &["plan", "--out", "infra/env/plan.json", "infra/env"],
        ),
    );

    let (saved_json, _) = read_saved_plan(&config.join("plan.json"));
    let nested = saved_json["compositions"]
        .as_array()
        .expect("saved compositions")
        .iter()
        .find(|composition| composition["binding"] == "wrapper.nested")
        .expect("saved nested secret composition");
    let root_location = saved_call_location(&nested["origin_id"]["root"]);
    let immediate_location = saved_call_location(
        nested["origin_id"]["descendants"]
            .as_array()
            .expect("saved descendants")
            .last()
            .expect("saved immediate call"),
    );

    let root_path = config.join("main.crn");
    let original = fs::read_to_string(&root_path).expect("read root configuration");
    fs::write(&root_path, format!("# root call moved\n{original}")).expect("move root call span");

    let apply = run_in_checkout(
        &checkout,
        &["apply", "--auto-approve", "infra/env/plan.json"],
    );
    assert!(
        !apply.status.success(),
        "a moved root call must invalidate secret rehydration"
    );
    let message = output_text(&apply);
    let expected_chain = format!("{root_location} -> {immediate_location}");
    for expected in [&expected_chain, "carina plan"] {
        assert!(
            message.contains(expected),
            "moved-call diagnostic must contain {expected:?}:\n{message}",
        );
    }
}

fn assert_relative_project_path_saved_plan_applies(project_arg: &str) {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("infra/foo");
    let module = project.join("module");
    fs::create_dir_all(&module).expect("create nested project");
    fs::write(
        project.join("main.crn"),
        r#"backend local { path = "carina.state.json" }
provider mock {}

let component = use { source = "./module" }
let instance = component {
  value = secret("RELATIVE_PROJECT_SECRET_4041")
}
"#,
    )
    .expect("write root configuration");
    fs::write(
        module.join("main.crn"),
        r#"arguments {
  value: String
}

let item = mock.test.resource {
  name    = "relative-project"
  comment = value
}
"#,
    )
    .expect("write module configuration");

    let mock_state = temp.path().join("mock-provider-state.json");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(temp.path())
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina from project parent")
    };

    assert_success("carina init relative project", &run(&["init", project_arg]));
    assert_success(
        "carina plan relative project",
        &run(&["plan", "--out", "plan.json", project_arg]),
    );

    let (_, saved_plan) = read_saved_plan(&temp.path().join("plan.json"));
    let origin = saved_plan
        .compositions
        .first()
        .expect("module call must produce a saved composition")
        .origin_id
        .root();
    assert_eq!(
        origin.source_file(),
        Path::new("main.crn"),
        "a relative project argument must not be duplicated inside its saved origin",
    );

    assert_success(
        "carina apply relative-project saved plan",
        &run(&["apply", "--auto-approve", "plan.json"]),
    );
}

#[test]
fn saved_plan_from_parent_non_dot_relative_project_path_applies() {
    assert_relative_project_path_saved_plan_applies("infra/foo");
}

#[test]
fn saved_plan_from_parent_dot_relative_project_path_applies() {
    assert_relative_project_path_saved_plan_applies("./infra/foo");
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
    let nested_wire = saved_json["compositions"]
        .as_array()
        .expect("serialized compositions")
        .iter()
        .find(|composition| composition["binding"] == "wrapper.nested")
        .expect("nested composition wire entry");
    assert_eq!(
        nested_wire["origin_id"]["descendants"]
            .as_array()
            .expect("saved origin descendants")
            .len(),
        1,
        "saved origin identity must retain the complete root-to-inner call path",
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

    for composition in saved_json["compositions"]
        .as_array()
        .expect("serialized compositions")
    {
        assert!(
            composition.get("origin_id").is_some(),
            "every saved composition must carry its required origin path: {composition}"
        );
        assert_eq!(
            composition["origin_id"]["root"]["source_file"]["location"], "source_root_relative",
            "project-local call identities must be normalized to the canonical plan root",
        );
        let serialized = composition.to_string();
        for forbidden in [
            "declared_type",
            "source_argument",
            "call_path",
            "quoted_string_attrs",
            "structural_operand",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "saved composition must omit live-only field {forbidden:?}: {serialized}",
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
            "saved-plan apply must not enter live composition validation; found {diagnostic_marker:?} in:\n{apply_text}",
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
