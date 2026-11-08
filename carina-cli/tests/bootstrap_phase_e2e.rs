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
            .env_remove("UNSET_X")
            .env_remove("UNSET_Y")
            .env_remove("MISSING")
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
fn init_defers_unconsumed_top_level_let_values() {
    let project = Project::new(&format!(
        "provider mock {{}}\nlet unused = env('{INPUT}')\n"
    ));
    let output = project.run(&["init", "."]);
    let message = rendered(&output);
    assert!(output.status.success(), "{message}");
}

#[test]
fn bootstrap_multifile_metadata_commands_defer_plain_and_secret_lets() {
    let project = Project::new("let item = mock.test.resource { name = 'item' }\n");
    fs::write(project.0.path().join("providers.crn"),
        "provider mock {}\nlet named = provider mock {}\nlet token = secret(env('UNSET_X'))\nlet unused = env('UNSET_Y')\n").unwrap();
    fs::write(
        project.0.path().join("backend.crn"),
        "backend local { path = 'state.json' }\n",
    )
    .unwrap();
    for arguments in [
        vec!["init", "."],
        vec!["fmt", "."],
        vec!["module", "list", "."],
    ] {
        let output = project.run(&arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            rendered(&output)
        );
    }
    for arguments in [["validate", "."], ["plan", "."]] {
        let output = project.run(&arguments);
        let message = rendered(&output);
        assert_eq!(output.status.code(), Some(1), "{message}");
        assert!(
            message.contains(carina_core::resource::INSIDE_SECRET_MESSAGE)
                || message.contains("environment variable 'UNSET_Y' is not set"),
            "{message}"
        );
        assert!(!message.contains("UNSET_X"), "{message}");
    }
}

#[test]
fn bootstrap_backend_secret_failure_is_located_and_masked() {
    let project = Project::new("let item = mock.test.resource { name = 'item' }\n");
    fs::write(project.0.path().join("providers.crn"), "provider mock {}\n").unwrap();
    fs::write(
        project.0.path().join("backend.crn"),
        "backend local { path = secret(env('UNSET_X')) }\n",
    )
    .unwrap();
    let output = project.run(&["init", "."]);
    let message = rendered(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("backend.crn:1:"), "{message}");
    assert!(
        message.contains("secret()- and decrypt()-derived values are not allowed"),
        "{message}"
    );
    assert!(!message.contains("UNSET_X"), "{message}");
}

#[test]
fn saved_apply_rejects_unavailable_current_backend_before_drift() {
    let project = Project::new("provider mock {}\nmock.test.resource { name = 'saved' }\n");
    fs::write(
        project.0.path().join("backend.crn"),
        "backend local { path = 'state.json' }\n",
    )
    .unwrap();
    for arguments in [
        vec!["init", "."],
        vec!["plan", "--refresh=false", "--out", "plan.json", "."],
    ] {
        let output = project.run(&arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            rendered(&output)
        );
    }
    fs::write(
        project.0.path().join("backend.crn"),
        "backend s3 { bucket = env(\"MISSING\") }\n",
    )
    .unwrap();
    let output = project.run(&["apply", "--auto-approve", "--lock=false", "plan.json"]);
    let message = rendered(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("backend.crn:1:"), "{message}");
    assert!(
        message.contains("environment variable 'MISSING' is not set"),
        "{message}"
    );
    assert!(!message.to_lowercase().contains("drift"), "{message}");
    assert!(!message.to_lowercase().contains("migration"), "{message}");
}
