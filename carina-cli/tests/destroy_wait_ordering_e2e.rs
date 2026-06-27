//! CLI-level regression coverage for destroy ordering through wait bindings.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

const DELETE_DELAY_MS: &str = "1500";

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    state_path: PathBuf,
    mock_state_path: PathBuf,
    delete_log_path: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            state_path: project.join("carina.state.json"),
            mock_state_path: project.join("mock-state.json"),
            delete_log_path: project.join("delete.log"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self) {
        self.write_config_with_cert_name("cert");
    }

    fn write_config_with_cert_name(&self, cert_name: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let cert = mock.test.resource {{
  name = "{cert_name}"
  status = "ISSUED"
}}

let cert_issued = wait cert {{
  until = cert.status == "ISSUED"
}}

let lst = mock.test.resource {{
  name = "lst"
  upstream = cert_issued.id
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
        self.seed_carina_state_with_cert_name("cert");
    }

    fn seed_carina_state_with_cert_name(&self, cert_name: &str) {
        let mut state = StateFile::new();
        let mut cert = ResourceState::new("test.resource", cert_name, "mock")
            .with_identifier("mock-id")
            .with_attribute("name", serde_json::json!(cert_name))
            .with_attribute("id", serde_json::json!("cert-id"))
            .with_attribute("status", serde_json::json!("ISSUED"));
        cert.binding = Some("cert".to_string());
        state.upsert_resource(cert);

        let mut lst = ResourceState::new("test.resource", "lst", "mock")
            .with_identifier("mock-id")
            .with_attribute("name", serde_json::json!("lst"))
            .with_attribute("id", serde_json::json!("lst-id"))
            .with_attribute("upstream", serde_json::json!("cert-id"));
        lst.binding = Some("lst".to_string());
        lst.dependency_bindings.insert("cert_issued".to_string());
        state.upsert_resource(lst);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_mock_provider_state(&self) {
        self.seed_mock_provider_state_with_cert_name("cert");
    }

    fn seed_mock_provider_state_with_cert_name(&self, cert_name: &str) {
        let provider_state = serde_json::json!({
            format!("test.resource.{cert_name}"): {
                "name": cert_name,
                "id": "cert-id",
                "status": "ISSUED"
            },
            "test.resource.lst": {
                "name": "lst",
                "id": "lst-id",
                "upstream": "cert-id"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn destroy(&self) -> Output {
        carina(&self.project)
            .args([
                "destroy",
                "--auto-approve",
                ".",
                "--lock=false",
                "--parallelism",
                "8",
            ])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .env("CARINA_MOCK_DELETE_LOG", &self.delete_log_path)
            .env("CARINA_MOCK_DELETE_DELAY_MS_FOR", "test.resource.lst")
            .env("CARINA_MOCK_DELETE_DELAY_MS", DELETE_DELAY_MS)
            .output()
            .expect("failed to execute carina destroy")
    }

    fn delete_log(&self) -> Vec<String> {
        fs::read_to_string(&self.delete_log_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn destroy_wait_ordering_uses_wait_target_binding_not_resource_name() {
    let scenario = Scenario::new();
    scenario.write_config_with_cert_name("primary-cert");
    scenario.init();
    scenario.seed_carina_state_with_cert_name("primary-cert");
    scenario.seed_mock_provider_state_with_cert_name("primary-cert");

    let output = scenario.destroy();
    assert_success("carina destroy", &output);

    let delete_log = scenario.delete_log();
    let lst_pos = delete_log
        .iter()
        .position(|entry| entry == "test.resource.lst")
        .expect("delete log should contain lst");
    let cert_pos = delete_log
        .iter()
        .position(|entry| entry == "test.resource.primary-cert")
        .expect("delete log should contain primary-cert");

    assert!(
        lst_pos < cert_pos,
        "destroy must delete lst before cert even when cert's resource name differs from its binding; delete log: {delete_log:?}"
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

#[test]
fn destroy_wait_ordering_deletes_dependent_before_wait_target() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();
    scenario.seed_carina_state();
    scenario.seed_mock_provider_state();

    let output = scenario.destroy();
    assert_success("carina destroy", &output);

    let delete_log = scenario.delete_log();
    let lst_pos = delete_log
        .iter()
        .position(|entry| entry == "test.resource.lst")
        .expect("delete log should contain lst");
    let cert_pos = delete_log
        .iter()
        .position(|entry| entry == "test.resource.cert")
        .expect("delete log should contain cert");

    assert!(
        lst_pos < cert_pos,
        "destroy must delete lst before cert when lst depends on wait binding cert_issued; delete log: {delete_log:?}"
    );
}
