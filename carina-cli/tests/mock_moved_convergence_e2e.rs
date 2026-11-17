//! Regression coverage for builtin mock object identity after state moves (#4468).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

struct Project {
    _temp: TempDir,
    root: PathBuf,
    mock_state: PathBuf,
}

impl Project {
    fn new(resources: &str) -> Self {
        let temp = tempfile::tempdir().expect("create moved convergence project");
        let root = temp.path().to_path_buf();
        fs::write(
            root.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write backend");
        fs::write(root.join("providers.crn"), "provider mock {}\n").expect("write provider");
        let project = Self {
            mock_state: root.join("mock-provider-state.json"),
            root,
            _temp: temp,
        };
        project.write("main.crn", resources);
        project.run(&["init", "."]);
        project.run(&["apply", ".", "--auto-approve"]);
        project
    }

    fn write(&self, file: &str, source: &str) {
        fs::write(self.root.join(file), source).expect("write project configuration");
    }

    fn run(&self, args: &[&str]) -> String {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .envs(env.iter().copied())
            .args(args)
            .output()
            .expect("run carina");
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            output.status.success(),
            "carina {args:?} failed\n{rendered}"
        );
        rendered
    }

    fn state(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.root.join("carina.state.json")).expect("read applied state"),
        )
        .expect("decode applied state")
    }

    fn apply_moves_and_assert_convergence(&self, count: usize) {
        let applied = self.run(&["apply", ".", "--auto-approve"]);
        assert!(
            applied.contains(&format!(
                "Plan: 0 to add, 0 to change, 0 to destroy, {count} to move."
            )),
            "apply must contain exactly the requested state moves\n{applied}",
        );
        let plan = self.run(&["plan", "."]);
        assert!(
            plan.contains("No changes"),
            "refresh after applied moves must find the existing mock objects\n{plan}",
        );
    }
}

#[test]
fn builtin_mock_moved_update_rekeys_identifier_and_delete_removes_object() {
    let project =
        Project::new("let a = mock.test.resource { name = 'original' comment = 'before' }\n");
    let before = project.state();
    let identifier = before["resources"][0]["current"]["identifier"]
        .as_str()
        .unwrap();
    project.write(
        "main.crn",
        "let b = mock.test.resource { name = 'original' comment = 'before' }\n",
    );
    project.write(
        "moved.crn",
        "moved { from = mock.test.resource 'a' to = mock.test.resource 'b' }\n",
    );
    project.apply_moves_and_assert_convergence(1);
    project.write(
        "main.crn",
        "let b = mock.test.resource { name = 'original' comment = 'after' }\n",
    );
    let updated = project.run_with_env(
        &["apply", ".", "--auto-approve"],
        &[(
            "CARINA_MOCK_UPDATE_RESULT_IDENTIFIER",
            "updated-provider-id",
        )],
    );
    assert!(
        updated.contains("0 to add, 1 to change, 0 to destroy"),
        "{updated}"
    );
    assert_eq!(
        project.state()["resources"][0]["current"]["identifier"],
        "updated-provider-id"
    );
    let objects: Value = serde_json::from_slice(&fs::read(&project.mock_state).unwrap()).unwrap();
    let objects = objects["states"].as_object().unwrap();
    assert_eq!(objects.len(), 1);
    assert!(!objects.contains_key(identifier));
    assert_eq!(objects["updated-provider-id"]["comment"], "after");
    let plan = project.run(&["plan", "."]);
    assert!(plan.contains("No changes"), "{plan}");
    project.run(&["destroy", ".", "--auto-approve"]);
    let objects: Value = serde_json::from_slice(&fs::read(&project.mock_state).unwrap()).unwrap();
    assert!(objects["states"].as_object().unwrap().is_empty());
}

#[test]
fn builtin_mock_named_move_converges_after_apply() {
    let project = Project::new("let a = mock.test.Thing { name = 'original' }\n");
    let before = project.state();
    project.write(
        "main.crn",
        "let b = mock.test.Thing { name = 'original' }\n",
    );
    project.write(
        "moved.crn",
        "moved { from = mock.test.Thing 'a' to = mock.test.Thing 'b' }\n",
    );

    project.apply_moves_and_assert_convergence(1);

    let after = project.state();
    assert_eq!(after["resources"][0]["identity"], "b");
    assert_eq!(
        after["resources"][0]["current"]["identifier"],
        before["resources"][0]["current"]["identifier"],
        "moving a state address must preserve the provider identifier",
    );
}

#[test]
fn builtin_mock_anonymous_to_let_moves_converge_after_apply() {
    // Current admission rejects multiple anonymous Things with mutable identity
    // inputs. Combine two independently applied anonymous objects to reproduce
    // the existing state that needs recovery in #4448.
    let project = Project::new("mock.test.Thing { name = 'first' }\n");
    let second = Project::new("mock.test.Thing { name = 'second' }\n");
    let mut state = project.state();
    state["resources"]
        .as_array_mut()
        .expect("first applied resources")
        .extend(
            second.state()["resources"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
    fs::write(
        project.root.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    let mut objects: Value =
        serde_json::from_slice(&fs::read(&project.mock_state).unwrap()).unwrap();
    let second_objects: Value =
        serde_json::from_slice(&fs::read(&second.mock_state).unwrap()).unwrap();
    objects["states"]
        .as_object_mut()
        .expect("first applied mock objects")
        .extend(second_objects["states"].as_object().unwrap().clone());
    fs::write(
        &project.mock_state,
        serde_json::to_vec_pretty(&objects).unwrap(),
    )
    .unwrap();
    let before = project.state();
    let rows = before["resources"].as_array().expect("applied resources");
    assert_eq!(rows.len(), 2);
    let mut moves = String::new();
    for (name, binding) in [("first", "a"), ("second", "b")] {
        let row = rows
            .iter()
            .find(|row| row["current"]["attributes"]["name"] == name)
            .expect("anonymous resource with the expected name");
        assert!(row["binding"].is_null());
        let identity = row["identity"].as_str().expect("anonymous state address");
        moves.push_str(&format!(
            "moved {{ from = mock.test.Thing '{identity}' to = mock.test.Thing '{binding}' }}\n"
        ));
    }
    project.write(
        "main.crn",
        "let a = mock.test.Thing { name = 'first' }\nlet b = mock.test.Thing { name = 'second' }\n",
    );
    project.write("moved.crn", &moves);

    project.apply_moves_and_assert_convergence(2);

    let after = project.state();
    let after_rows = after["resources"].as_array().expect("moved resources");
    assert_eq!(after_rows.len(), 2);
    for row in rows {
        let moved = after_rows
            .iter()
            .find(|moved| moved["current"]["identifier"] == row["current"]["identifier"])
            .expect("move must preserve each provider identifier");
        assert!(matches!(moved["identity"].as_str(), Some("a" | "b")));
    }
}
