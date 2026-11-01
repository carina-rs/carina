use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::resource::{ConcreteValue, Resource, Value};
use indexmap::IndexMap;

fn tags<S, D>(resource: &Resource<S, D>) -> &IndexMap<String, Value> {
    match resource.get_attr("tags") {
        Some(Value::Concrete(ConcreteValue::Map(tags))) => tags,
        other => panic!(
            "expected concrete tags map on {}, got {other:?}",
            resource.id
        ),
    }
}

struct PlanJsonOutput {
    plan: PlanFile,
    json: serde_json::Value,
    stderr: String,
}

fn run_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "carina {operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn plan_json(dir: &Path) -> PlanJsonOutput {
    let init = run_cli(dir, &["init", "."]);
    assert_success(&init, "init");

    let output = run_cli(dir, &["plan", "--json", "."]);
    assert_success(&output, "plan --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json_start = stdout
        .find('{')
        .expect("JSON plan must follow provider and refresh status lines");
    let json: serde_json::Value =
        serde_json::from_str(&stdout[json_start..]).unwrap_or_else(|error| {
        panic!(
            "plan output was not a saved-plan JSON document: {error}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )
    });
    let plan = serde_json::from_value(json.clone()).expect("decode saved-plan JSON");

    PlanJsonOutput { plan, json, stderr }
}

#[test]
fn plan_scopes_default_tags_to_the_routed_provider_instance() {
    let project = tempfile::tempdir().expect("create temporary project");
    std::fs::write(
        project.path().join("providers.crn"),
        r#"provider mock {
  default_tags = {
    Inst = "default"
  }
}

let second = provider mock {
  default_tags = {
    Inst = "second"
    Only = "second"
  }
}
"#,
    )
    .expect("write provider instances");
    std::fs::write(
        project.path().join("main.crn"),
        r#"let default_resource = mock.test.resource {
  name = "default-resource"
}

let second_resource = mock.test.resource {
  name = "second-resource"

  directives {
    provider = second
  }
}
"#,
    )
    .expect("write routed resources");

    let PlanJsonOutput { plan, .. } = plan_json(project.path());
    let default_resource = plan
        .sorted_resources
        .iter()
        .find(|resource| resource.id.provider_instance.is_none())
        .expect("default-instance resource must be present");
    let second_resource = plan
        .sorted_resources
        .iter()
        .find(|resource| resource.id.provider_instance.as_deref() == Some("second"))
        .expect("named-instance resource must be present");

    assert_eq!(
        tags(default_resource),
        &IndexMap::from([(
            "Inst".to_string(),
            Value::Concrete(ConcreteValue::String("default".into())),
        )]),
        "default-instance resources must not receive the named instance's tags"
    );
    assert_eq!(
        tags(second_resource),
        &IndexMap::from([
            (
                "Inst".to_string(),
                Value::Concrete(ConcreteValue::String("second".into())),
            ),
            (
                "Only".to_string(),
                Value::Concrete(ConcreteValue::String("second".into())),
            ),
        ]),
        "named-instance resources must receive only that instance's tags"
    );
}

#[test]
fn plan_merges_default_tags_into_a_ref_bearing_tags_map() {
    let project = tempfile::tempdir().expect("create temporary project");
    std::fs::write(
        project.path().join("providers.crn"),
        r#"provider mock {
  default_tags = {
    ManagedBy = "carina"
  }
}
"#,
    )
    .expect("write provider configuration");
    std::fs::write(
        project.path().join("main.crn"),
        r#"let a = mock.test.resource {
  name = "a"
}

let b = mock.test.resource {
  name = "b"
  tags = {
    Name = "b"
    Peer = a.identifier
  }
}
"#,
    )
    .expect("write ref-bearing resources");

    let PlanJsonOutput { plan, json, .. } = plan_json(project.path());
    let resource_index = plan
        .sorted_resources
        .iter()
        .position(|resource| resource.binding.as_deref() == Some("b"))
        .expect("resource b must be present");
    let resource = &plan.sorted_resources[resource_index];

    assert_eq!(
        tags(resource).get("ManagedBy"),
        Some(&Value::Concrete(ConcreteValue::String("carina".into()))),
        "the plan must include provider defaults in a ref-bearing tags map"
    );
    assert!(tags(resource).contains_key("Peer"));
    assert!(
        resource.default_tag_keys.contains("ManagedBy"),
        "the plan must record the tag supplied by provider defaults"
    );
    let json_resource = &json["sorted_resources"][resource_index];
    assert_eq!(
        json_resource.get("default_tag_keys"),
        Some(&serde_json::json!(["ManagedBy"])),
        "plan JSON must serialize typed default-tag provenance"
    );
    assert!(
        json_resource["attributes"]
            .as_object()
            .expect("resource attributes must be an object")
            .keys()
            .all(|key| !key.starts_with('_')),
        "plan JSON attributes must contain no host metadata"
    );
}

#[test]
fn saved_plan_apply_preserves_default_tag_annotation_without_persisting_provenance() {
    let project = tempfile::tempdir().expect("create temporary project");
    fs::write(
        project.path().join("backend.crn"),
        r#"backend local { path = "carina.state.json" }
"#,
    )
    .expect("write backend configuration");
    fs::write(
        project.path().join("providers.crn"),
        r#"provider mock {
  default_tags = {
    ManagedBy = "carina"
  }
}
"#,
    )
    .expect("write provider configuration");
    fs::write(
        project.path().join("main.crn"),
        r#"let a = mock.test.resource {
  name = "a"
}
"#,
    )
    .expect("write resource configuration");

    assert_success(&run_cli(project.path(), &["init", "."]), "init");
    assert_success(
        &run_cli(project.path(), &["plan", "--out", "plan.json", "."]),
        "plan --out",
    );

    let saved_plan: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(project.path().join("plan.json")).expect("read saved plan"),
    )
    .expect("parse saved plan");
    let saved_resource = saved_plan["sorted_resources"]
        .as_array()
        .expect("saved plan resources must be an array")
        .iter()
        .find(|resource| resource["binding"] == "a")
        .expect("saved plan must contain resource a");
    assert_eq!(
        saved_resource.get("default_tag_keys"),
        Some(&serde_json::json!(["ManagedBy"])),
        "saved plan must preserve default-tag provenance"
    );

    let apply = run_cli(project.path(), &["apply", "--auto-approve", "plan.json"]);
    assert_success(&apply, "apply saved plan");
    let stdout = String::from_utf8_lossy(&apply.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("ManagedBy: 'carina'  # default_tags")),
        "saved-plan apply must annotate the default tag; stdout:\n{stdout}"
    );

    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(project.path().join("carina.state.json"))
            .expect("read persisted state"),
    )
    .expect("parse persisted state");
    let attributes = state["resources"][0]["current"]["attributes"]
        .as_object()
        .expect("persisted resource attributes must be an object");
    assert!(
        attributes.keys().all(|key| !key.starts_with('_')),
        "persisted provider state must contain no host metadata: {attributes:#?}"
    );
}

#[test]
fn apply_updates_default_tags_without_persisting_host_metadata() {
    let project = tempfile::tempdir().expect("create temporary project");
    fs::write(
        project.path().join("backend.crn"),
        r#"backend local { path = "carina.state.json" }
"#,
    )
    .expect("write backend configuration");
    let provider_path = project.path().join("providers.crn");
    fs::write(
        &provider_path,
        r#"provider mock {
  default_tags = {
    ManagedBy = "carina"
  }
}
"#,
    )
    .expect("write provider configuration");
    fs::write(
        project.path().join("main.crn"),
        r#"mock.test.resource {
  name = "tagged"
}
"#,
    )
    .expect("write resource configuration");

    assert_success(&run_cli(project.path(), &["init", "."]), "init");

    let assert_state = |expected_tags: serde_json::Value| {
        let state: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(project.path().join("carina.state.json"))
                .expect("read persisted state"),
        )
        .expect("parse persisted state");
        let attributes = state["resources"][0]["current"]["attributes"]
            .as_object()
            .expect("persisted resource attributes must be an object");
        assert!(
            attributes.keys().all(|key| !key.starts_with('_')),
            "persisted provider state must contain no host metadata: {attributes:#?}"
        );
        assert_eq!(attributes.get("tags"), Some(&expected_tags));
    };

    assert_success(
        &run_cli(project.path(), &["apply", ".", "--auto-approve"]),
        "apply",
    );
    assert_state(serde_json::json!({ "ManagedBy": "carina" }));

    fs::write(
        provider_path,
        r#"provider mock {
  default_tags = {
    ManagedBy = "carina"
    Team = "x"
  }
}
"#,
    )
    .expect("update provider configuration");

    let plan = run_cli(project.path(), &["plan", "."]);
    assert_success(&plan, "plan");
    let stdout = String::from_utf8_lossy(&plan.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.trim() == "+ Team: 'x'  # default_tags"),
        "update plan must annotate the newly injected default tag; stdout:\n{stdout}"
    );

    assert_success(
        &run_cli(project.path(), &["apply", ".", "--auto-approve"]),
        "apply",
    );
    assert_state(serde_json::json!({
        "ManagedBy": "carina",
        "Team": "x"
    }));
}

#[test]
fn plan_logs_when_an_unresolved_default_tag_is_skipped() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    let missing = root.path().join("missing");
    std::fs::create_dir_all(&project).expect("create project directory");
    std::fs::create_dir_all(&missing).expect("create upstream directory");
    std::fs::write(
        missing.join("main.crn"),
        r#"backend local { path = "carina.state.json" }"#,
    )
    .expect("write upstream configuration");
    std::fs::write(
        project.join("main.crn"),
        r#"backend local { path = "carina.state.json" }

let mgmt = upstream_state { source = "../missing" }

provider mock {
  default_tags = {
    Owner = mgmt.owner
  }
}

mock.test.resource {
  name = "tagged"
}
"#,
    )
    .expect("write project configuration");

    let PlanJsonOutput { stderr, .. } = plan_json(&project);
    assert!(
        stderr.contains("Skipping default_tag 'Owner'"),
        "missing skipped-default-tag log message; stderr:\n{stderr}"
    );
}
