//! Bootstrap needs installation/backend metadata, not resource attribute values.

use std::{
    fs,
    process::{Command, Output},
};

const INPUT: &str = "CARINA_3979_BOOTSTRAP_MISSING_INPUT";
const METADATA: &str = "CARINA_3979_BOOTSTRAP_MISSING_METADATA";

struct Project(tempfile::TempDir);

impl Project {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("main.crn"), source).unwrap();
        Self(directory)
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.0.path())
            .env_remove(INPUT)
            .env_remove(METADATA)
            .env("NO_COLOR", "1")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.0.path().join("mock-state.json"),
            )
            .args(arguments)
            .output()
            .unwrap()
    }
}

fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn init_defers_missing_non_secret_resource_input_while_validate_and_plan_reject_it() {
    let project = Project::new(&format!(
        "provider mock {{}}\nlet item = mock.test.resource {{ name = 'item' comment = env('{INPUT}') }}\n"
    ));
    let init = project.run(&["init", "."]);
    assert!(init.status.success(), "{}", rendered(&init));
    for arguments in [["validate", "."], ["plan", "."]] {
        let output = project.run(&arguments);
        let message = rendered(&output);
        assert!(!output.status.success(), "{message}");
        assert!(
            message.contains(&format!("env(): environment variable '{INPUT}' is not set")),
            "{message}"
        );
    }
}

#[test]
fn init_defers_missing_secret_resource_input_without_weakening_validate_or_plan() {
    let project = Project::new(&format!(
        "provider mock {{}}\nlet item = mock.test.resource {{ name = 'item' comment = secret(env('{INPUT}')) }}\n"
    ));
    let init = project.run(&["init", "."]);
    assert!(init.status.success(), "{}", rendered(&init));
    for arguments in [["validate", "."], ["plan", "."]] {
        let output = project.run(&arguments);
        let message = rendered(&output);
        assert!(!output.status.success(), "{message}");
        assert!(
            message.contains(carina_core::resource::INSIDE_SECRET_MESSAGE),
            "{message}"
        );
        assert!(!message.contains(INPUT), "{message}");
    }
}

#[test]
fn init_still_rejects_unavailable_backend_metadata() {
    let project = Project::new(&format!(
        "provider mock {{}}\nbackend local {{ path = env('{METADATA}') }}\nlet item = mock.test.resource {{ name = 'item' comment = env('{INPUT}') }}\n"
    ));
    let output = project.run(&["init", "."]);
    let message = rendered(&output);
    assert!(!output.status.success(), "{message}");
    assert!(
        message.contains(&format!(
            "env(): environment variable '{METADATA}' is not set"
        )),
        "{message}"
    );
    assert!(!message.contains(INPUT), "{message}");
}

#[test]
fn init_still_rejects_unavailable_provider_installation_metadata() {
    for attribute in ["source", "version"] {
        let project = Project::new(&format!(
            "provider mock {{ {attribute} = env('{METADATA}') }}\nlet item = mock.test.resource {{ name = 'item' comment = env('{INPUT}') }}\n"
        ));
        let output = project.run(&["init", "."]);
        let message = rendered(&output);
        assert!(!output.status.success(), "{attribute}: {message}");
        assert!(!message.contains(INPUT), "{attribute}: {message}");
        assert!(message.contains(attribute), "{attribute}: {message}");
    }
}

#[test]
fn init_still_rejects_unavailable_top_level_let_values() {
    let project = Project::new(&format!(
        "provider mock {{}}\nlet unused = env('{INPUT}')\n"
    ));
    let output = project.run(&["init", "."]);
    let message = rendered(&output);
    assert!(!output.status.success(), "{message}");
    assert!(
        message.contains(&format!("env(): environment variable '{INPUT}' is not set")),
        "{message}"
    );
}
