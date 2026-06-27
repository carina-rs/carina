use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    state_path: PathBuf,
    mock_state_path: PathBuf,
    op_log_path: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            state_path: project.join("carina.state.json"),
            mock_state_path: project.join("mock-state.json"),
            op_log_path: project.join("op.log"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let r1 = mock.test.resource {{
  name = "replacement"
}}
"#,
                self.state_path.display()
            ),
        )
        .unwrap();
    }

    fn init(&self) {
        let output = carina(&self.project)
            .args(["init", "."])
            .output()
            .expect("failed to execute carina init");
        assert_success("carina init", &output);
    }

    fn seed_carina_state(&self) {
        let mut state = StateFile::new();
        let mut resource = ResourceState::new("test.resource", "r1", "mock")
            .with_identifier("old-r1-id")
            .with_attribute("name", serde_json::json!("original"));
        resource.binding = Some("r1".to_string());
        state.upsert_resource(resource);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_mock_provider_state(&self) {
        let provider_state = serde_json::json!({
            "test.resource.r1": {
                "name": "original"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn apply_with_create_failure(&self) -> Output {
        carina(&self.project)
            .args([
                "apply",
                ".",
                "--auto-approve",
                "--lock=false",
                "--parallelism",
                "1",
            ])
            .env("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.r1")
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_OP_LOG", &self.op_log_path)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .output()
            .expect("failed to execute carina apply")
    }
}

#[test]
fn test_dbd_create_failure_cleans_up_old_state() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();
    scenario.seed_carina_state();
    scenario.seed_mock_provider_state();

    let output = scenario.apply_with_create_failure();
    assert!(
        !output.status.success(),
        "apply must fail when mock create fails\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let state: StateFile =
        serde_json::from_str(&fs::read_to_string(&scenario.state_path).unwrap()).unwrap();
    assert!(
        state.find_resource("mock", "test.resource", "r1").is_none(),
        "DBD delete success + create failure must remove stale state row:\n{}",
        fs::read_to_string(&scenario.state_path).unwrap()
    );

    let op_log = fs::read_to_string(&scenario.op_log_path).unwrap();
    assert!(
        op_log.contains("delete test.resource.r1")
            && op_log.contains("create-fail test.resource.r1"),
        "operation log should show delete succeeded before create failure:\n{op_log}"
    );
}

fn carina(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
