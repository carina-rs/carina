use std::path::Path;
use std::process::Command;

use carina_cli::commands::plan::PlanFile;
use carina_core::resource::{ConcreteValue, Resource, Value};
use indexmap::IndexMap;

fn tags(resource: &Resource) -> &IndexMap<String, Value> {
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
    stderr: String,
}

fn plan_json(dir: &Path) -> PlanJsonOutput {
    let init = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(["init", "."])
        .output()
        .expect("run carina init");
    assert!(
        init.status.success(),
        "carina init failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(["plan", "--json", "."])
        .output()
        .expect("run carina plan");
    assert!(
        output.status.success(),
        "carina plan failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json_start = stdout
        .find('{')
        .expect("JSON plan must follow provider and refresh status lines");
    let plan = serde_json::from_str(&stdout[json_start..]).unwrap_or_else(|error| {
        panic!(
            "plan output was not a saved-plan JSON document: {error}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )
    });

    PlanJsonOutput { plan, stderr }
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

    let PlanJsonOutput { plan, .. } = plan_json(project.path());
    let resource = plan
        .sorted_resources
        .iter()
        .find(|resource| resource.binding.as_deref() == Some("b"))
        .expect("resource b must be present");

    assert_eq!(
        tags(resource).get("ManagedBy"),
        Some(&Value::Concrete(ConcreteValue::String("carina".into()))),
        "the plan must include provider defaults in a ref-bearing tags map"
    );
    assert!(tags(resource).contains_key("Peer"));
    assert_eq!(
        resource.get_attr("_default_tag_keys"),
        Some(&Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("ManagedBy".into())),
        ]))),
        "the plan must record exactly which tag was supplied by provider defaults"
    );
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
