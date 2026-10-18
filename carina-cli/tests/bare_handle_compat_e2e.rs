//! Compatibility regressions for legacy bare structural-handle transport.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CARINA_MOCK_STATE_FILE")
        .args(args)
        .output()
        .expect("run carina")
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

#[test]
fn bare_handles_survive_apply_and_replan_without_a_permanent_diff() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "modules/compat/main.crn",
        r#"arguments {
  parent: String
}

let internal = mock.test.resource {
  name = "module-internal"
  parent = parent
}

attributes {
  handle: String = internal
}
"#,
    );
    write_file(
        fixture.path(),
        "main.crn",
        r#"backend local { path = "carina.state.json" }
provider mock {}

let vpc = mock.test.resource {
  name = "vpc"
}

let auxiliary = mock.test.resource {
  name = "auxiliary"
}

let ready = wait vpc {
  until = vpc.identifier == "vpc-id"
}

let compat = use { source = "./modules/compat" }
let module_instance = compat { parent = vpc }

let direct = mock.test.resource {
  name = "direct"
  parent = vpc
}

let indexed_consumers = for (index, handle) in [vpc] {
  mock.test.resource {
    name = "indexed-consumer"
    parent = handle
  }
}

let waited_consumer = mock.test.resource {
  name = "waited-consumer"
  parent = ready.identifier
}

let module_output_consumer = mock.test.resource {
  name = "module-output-consumer"
  parent = module_instance.handle
}

let collections = mock.example.consumer {
  name = "collections"
  list = [vpc, auxiliary]
  nested = { primary = vpc, secondary = auxiliary }
}

exports {
  handle: String = vpc
}
"#,
    );

    assert_success("carina init", &run(fixture.path(), &["init", "."]));
    let apply = run(
        fixture.path(),
        &["apply", ".", "--auto-approve", "--lock=false"],
    );
    assert_success("carina apply", &apply);

    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.path().join("carina.state.json"))
            .expect("read persisted state"),
    )
    .expect("valid state JSON");
    assert_eq!(state["exports"]["handle"], "${vpc}");
    let resources = state["resources"].as_array().expect("state resources");
    let attributes = |binding: &str| {
        &resources
            .iter()
            .find(|resource| resource["binding"] == binding)
            .unwrap_or_else(|| panic!("state resource {binding:?}"))["attributes"]
    };
    assert_eq!(attributes("direct")["parent"], "${vpc}");
    assert_eq!(attributes("indexed_consumers[0]")["parent"], "${vpc}");
    assert_eq!(attributes("waited_consumer")["parent"], "vpc-id");
    assert_eq!(
        attributes("module_output_consumer")["parent"],
        "${internal}"
    );
    assert_eq!(attributes("module_instance.internal")["parent"], "${vpc}");
    assert_eq!(
        attributes("collections")["list"],
        serde_json::json!(["${vpc}", "${auxiliary}"])
    );
    assert_eq!(
        attributes("collections")["nested"],
        serde_json::json!({
            "primary": "${vpc}",
            "secondary": "${auxiliary}",
        })
    );

    let replan = run(fixture.path(), &["plan", "."]);
    let replan_text = output_text(&replan);
    assert!(
        replan.status.success()
            && replan_text.contains("No changes. Infrastructure is up-to-date."),
        "bare handles must compare byte-for-byte with provider/state values after apply:\n{replan_text}",
    );
}
