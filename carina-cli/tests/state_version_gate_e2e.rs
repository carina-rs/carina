//! Full-state commands reject unsupported state versions without rewriting their bytes.
//! Destroy and refresh handle retyped bindings with v21 orphans.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Project {
    temp: TempDir,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let project = Self { temp, root };
        project.write("providers.crn", "provider mock {}\n");
        project.write(
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        );
        project
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", self.temp.path().join("mock.json"))
            .env(
                "CARINA_MOCK_OP_LOG",
                self.temp.path().join("operations.log"),
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("carina.state.json")
    }

    fn state(&self) -> Value {
        read_json(&self.state_path())
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", rendered(output));
}

fn retyped_binding(command: &[&str]) {
    let project = Project::new();
    project.write(
        "main.crn",
        "let vpc = mock.test.resource { name = 'original' }\nlet subnet = mock.test.resource { name = 'subnet' parent = vpc.name }\n",
    );
    success(&project.run(&["init", "."]));
    success(&project.run(&["apply", "--auto-approve", "."]));
    project.write(
        "main.crn",
        "let vpc = mock.test.Thing { name = 'replacement' }\nlet subnet = mock.test.resource { name = 'subnet' parent = vpc.name }\n",
    );
    success(&project.run(&["plan", "."]));
    success(&project.run(command));
    assert_eq!(
        project.state()["version"],
        carina_state::StateFile::CURRENT_VERSION
    );
}

#[test]
fn destroy_retyped_binding_with_v21_orphan() {
    retyped_binding(&["destroy", "--auto-approve", "."]);
}

#[test]
fn refresh_retyped_binding_with_v21_orphan() {
    retyped_binding(&["state", "refresh", "."]);
}

#[test]
fn plan_and_apply_reject_v20_local_state_without_rewriting_it() {
    let project = Project::new();
    project.write(
        "main.crn",
        "let example = mock.test.resource { name = 'example' }\n",
    );
    success(&project.run(&["init", "."]));
    let bytes = serde_json::to_vec_pretty(&json!({
        "version": 20, "exports_version": 1,
        "serial": 7, "lineage": "unsupported-state", "carina_version": "0.15.0",
        "resources": [], "exports": {}
    }))
    .unwrap();
    fs::write(project.state_path(), &bytes).unwrap();
    for args in [vec!["plan", "."], vec!["apply", "--auto-approve", "."]] {
        let output = project.run(&args);
        assert!(!output.status.success(), "{}", rendered(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        for required in ["version 20", "21", "carina v0.15.0"] {
            assert!(stderr.contains(required), "{stderr}");
        }
        assert_eq!(fs::read(project.state_path()).unwrap(), bytes);
    }
}
