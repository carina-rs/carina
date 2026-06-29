//! CLI-level regression coverage for CBD consumer update ordering.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

const UPDATE_DELAY_MS: &str = "200";

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

let web_acl = mock.test.resource {{
  name = "web-acl-new"
  comment = "v2"
}}

let distribution = mock.test.resource {{
  name = "dist"
  comment = "dist-v2"
  web_acl_arn = web_acl.identifier
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
        let mut web_acl = ResourceState::new("test.resource", "web_acl", "mock")
            .with_identifier("web-acl-old-id")
            .with_attribute("name", serde_json::json!("web-acl-old"))
            .with_attribute("identifier", serde_json::json!("web-acl-old-id"))
            .with_attribute("comment", serde_json::json!("v1"));
        web_acl.binding = Some("web_acl".to_string());
        state.upsert_resource(web_acl);

        let mut distribution = ResourceState::new("test.resource", "distribution", "mock")
            .with_identifier("dist-id")
            .with_attribute("name", serde_json::json!("dist"))
            .with_attribute("identifier", serde_json::json!("dist-id"))
            .with_attribute("comment", serde_json::json!("dist-v1"))
            .with_attribute("web_acl_arn", serde_json::json!("web-acl-old-id"));
        distribution.binding = Some("distribution".to_string());
        distribution
            .dependency_bindings
            .insert("web_acl".to_string());
        state.upsert_resource(distribution);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_mock_provider_state(&self) {
        let provider_state = serde_json::json!({
            "test.resource.web_acl": {
                "name": "web-acl-old",
                "identifier": "web-acl-old-id",
                "comment": "v1"
            },
            "test.resource.distribution": {
                "name": "dist",
                "identifier": "dist-id",
                "comment": "dist-v1",
                "web_acl_arn": "web-acl-old-id"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn apply(&self) -> Output {
        carina(&self.project)
            .args([
                "apply",
                "--parallelism",
                "8",
                "--auto-approve",
                "--lock=false",
                ".",
            ])
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .env("CARINA_MOCK_OP_LOG", &self.op_log_path)
            .env("CARINA_MOCK_UPDATE_DELAY_MS", UPDATE_DELAY_MS)
            .env_remove("CARINA_MOCK_CREATE_FAIL_FOR")
            .output()
            .expect("failed to execute carina apply")
    }

    fn op_log(&self) -> Vec<String> {
        fs::read_to_string(&self.op_log_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn cbd_replace_orders_consumer_update_between_create_and_delete() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();
    scenario.seed_carina_state();
    scenario.seed_mock_provider_state();

    let output = scenario.apply();
    assert_success("carina apply", &output);

    let op_log = scenario.op_log();
    let create_web_acl_pos = op_position(
        &op_log,
        "create test.resource.web_acl",
        "new web ACL create",
    );
    let update_dist_pos = op_position(
        &op_log,
        "update test.resource.distribution",
        "distribution consumer update",
    );
    let delete_web_acl_pos = op_position(
        &op_log,
        "delete test.resource.web_acl",
        "old web ACL delete",
    );

    assert!(
        create_web_acl_pos < update_dist_pos && update_dist_pos < delete_web_acl_pos,
        "Issue #3625 regression: CBD replacement must order the consumer Update between \
         the new producer Create and the old producer Delete; expected create web_acl -> \
         update distribution -> delete web_acl, op log: {op_log:?}"
    );
}

fn op_position(op_log: &[String], expected: &str, label: &str) -> usize {
    op_log
        .iter()
        .position(|entry| entry == expected)
        .unwrap_or_else(|| {
            panic!(
                "Issue #3625 ordering test must observe {label} (`{expected}`); op log: {op_log:?}"
            )
        })
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
