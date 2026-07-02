//! Regression coverage for carina#3676.
//!
//! `carina plan <dir>` must resolve a relative local backend state path
//! against `<dir>`, not the caller's current working directory. A foreign
//! cwd used to make plan miss the existing state and report phantom creates.

use std::fs;
use std::process::{Command, Output};

use carina_core::resource::{ConcreteValue, Value};
use carina_state::{BackendLock, ResourceState, StateFile};
use tempfile::TempDir;

fn plain_text_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    cmd
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn local_backend_lock(path: &str) -> BackendLock {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String(path.to_string())),
    );
    let config = carina_core::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes,
    };
    BackendLock::for_config(Some(&config)).unwrap()
}

fn state_json() -> String {
    let mut state = StateFile::new();
    state.serial = 3;
    state.lineage = "plan-state-anchoring".to_string();
    state.upsert_resource(
        ResourceState::new("test.resource", "r1", "mock")
            .with_identifier("mock-id")
            .with_attribute("name", serde_json::json!("r1")),
    );
    serde_json::to_string_pretty(&state).expect("serialize state fixture")
}

#[test]
fn plan_path_argument_reads_local_state_from_config_dir_when_cwd_differs() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let foreign_cwd = tmp.path().join("elsewhere");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&foreign_cwd).unwrap();

    fs::write(
        project.join("backend.crn"),
        r#"backend local { path = "state.json" }
"#,
    )
    .unwrap();
    fs::write(
        project.join("main.crn"),
        r#"let r1 = mock.test.resource { name = "r1" }
"#,
    )
    .unwrap();
    fs::write(project.join("state.json"), state_json()).unwrap();
    local_backend_lock("state.json").save(&project).unwrap();

    let output = plain_text_command(env!("CARGO_BIN_EXE_carina"))
        .current_dir(&foreign_cwd)
        .args([
            "plan",
            "--refresh=false",
            "--detailed-exitcode",
            project.to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute carina plan");

    assert!(
        output.status.success(),
        "plan should return detailed-exitcode 0 when the target-dir state matches.\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output),
    );
    let stdout = stdout(&output);
    assert!(
        stdout.contains("No changes. Infrastructure is up-to-date."),
        "plan must read state from the config dir and see the existing resource.\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("Create mock.test.resource") && !stdout.contains("+ mock.test.resource"),
        "plan must not report a phantom create from a cwd-relative empty state.\nstdout:\n{stdout}"
    );
}
