//! Probe parser-vs-saved-plan legacy data-source pruning before source equality.
use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Output},
};

fn assert_success(label: &str, output: Output) {
    assert!(
        output.status.success(),
        "{label}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn probe(deferred: bool) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    fs::create_dir(project.join("queries")).unwrap();
    fs::write(project.join("providers.crn"), "provider mock {}\n").unwrap();
    fs::write(
        project.join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .unwrap();
    fs::write(
        project.join("resources.crn"),
        r#"let origin = mock.test.resource { name = "origin" }
let consumer = mock.test.resource { name = "consumer" parent = origin.name }
"#,
    )
    .unwrap();
    fs::write(
        project.join("queries/arguments.crn"),
        "arguments { pattern: String }\n",
    )
    .unwrap();
    fs::write(
        project.join("queries/data.crn"),
        "let roles = read mock.iam.Roles { name_regex = pattern }\n",
    )
    .unwrap();
    fs::write(
        project.join("main.crn"),
        r#"let query_module = use { source = "./queries" }
let child = query_module { pattern = "origin" }
"#,
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", project.join("mock-state.json"))
            .args(args)
            .output()
            .unwrap()
    };
    assert_success("init", run(&["init", "."]));
    assert_success(
        "seed",
        run(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let state_path = project.join("carina.state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["version"] = json!(20);
    for row in state["resources"].as_array_mut().unwrap() {
        row["dependency_bindings"] = Value::Array(
            row["dependency_bindings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|dependency| dependency["identity"].clone())
                .collect(),
        );
    }
    state["resources"].as_array_mut().unwrap().push(json!({
        "provider": "mock", "resource_type": "iam.Roles", "identity": "child.roles",
        "binding": "child.roles", "dependency_bindings": ["origin"],
        "current": {"kind": "missing_identifier", "attributes": {"names": ["historical"]}, "nested_write_only": {}}
    }));
    fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    if deferred {
        fs::write(
            project.join("main.crn"),
            r#"let query_module = use { source = "./queries" }
let next = mock.test.resource { name = "next" }
let child = query_module { pattern = next.name }
"#,
        )
        .unwrap();
    }
    let before = fs::read(&state_path).unwrap();
    assert_success(
        "save legacy plan",
        run(&["plan", "--out", "plan.json", "."]),
    );
    assert!(
        fs::read(&state_path).unwrap() == before,
        "plan must leave legacy state untouched"
    );
    let plan: Value =
        serde_json::from_slice(&fs::read(project.join("plan.json")).unwrap()).unwrap();
    if deferred {
        assert!(
            plan["plan"]["effects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|effect| effect.get("Read").is_some()),
            "probe must retain an apply-time deferred read: {plan:#}"
        );
    }
    assert_success(
        "apply saved legacy classification",
        run(&["apply", "--auto-approve", "--lock=false", "plan.json"]),
    );
    let state: Value = serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
    assert_eq!(state["version"], 21);
    assert!(
        state["resources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["identity"] != "child.roles")
    );
}

#[test]
fn legacy_module_data_source_rows_match_saved_migration_sources() {
    probe(false);
}

#[test]
fn legacy_deferred_module_data_source_rows_match_saved_migration_sources() {
    probe(true);
}
