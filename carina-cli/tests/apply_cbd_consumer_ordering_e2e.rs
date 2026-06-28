//! CLI-level regression coverage for CBD replace consumer update ordering.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

const UPDATE_DELAY_MS: &str = "1500";

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
  identifier = "new-web-acl-id"
  directives {{ create_before_destroy = true }}
}}

let distribution = mock.test.resource {{
  name = "distribution"
  comment = "after"
  web_acl_arn = web_acl.identifier
}}
"#,
                self.state_path.display()
            ),
        )
        .unwrap();
    }

    fn write_rename_config(&self) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let renamed = mock.test.renameable_resource {{
  name = "rename-target"
  force_replace = "new"
  directives {{ create_before_destroy = true }}
}}
"#,
                self.state_path.display()
            ),
        )
        .unwrap();
    }

    fn write_rename_consumer_config(&self) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let renamed = mock.test.renameable_resource {{
  name = "rename-target"
  force_replace = "new"
  directives {{ create_before_destroy = true }}
}}

let consumer = mock.test.resource {{
  name = "consumer"
  comment = renamed.name
}}
"#,
                self.state_path.display()
            ),
        )
        .unwrap();
    }

    fn write_permanent_name_config(&self) {
        self.write_permanent_name_config_with_name("stable-name");
    }

    fn write_permanent_name_config_with_name(&self, name: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let permanent = mock.test.resource {{
  name = "{name}"
  force_replace = "new"
  directives {{ create_before_destroy = true }}
}}
"#,
                self.state_path.display(),
                name = name
            ),
        )
        .unwrap();
    }

    fn write_interpolated_permanent_name_config(&self, prefix: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let prefix = "{prefix}"

let permanent = mock.test.resource {{
  name = "${{prefix}}-name"
  force_replace = "new"
  directives {{ create_before_destroy = true }}
}}
"#,
                self.state_path.display(),
                prefix = prefix
            ),
        )
        .unwrap();
    }

    fn write_ref_permanent_name_config(&self, source_name: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "{}" }}
provider mock {{}}

let source = mock.test.renameable_resource {{
  name = "{source_name}"
}}

let permanent = mock.test.resource {{
  name = source.name
  force_replace = "new"
  directives {{ create_before_destroy = true }}
}}
"#,
                self.state_path.display(),
                source_name = source_name
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
            .with_identifier("old-web-acl-id")
            .with_attribute("name", serde_json::json!("web-acl-old"))
            .with_attribute("identifier", serde_json::json!("old-web-acl-id"));
        web_acl.binding = Some("web_acl".to_string());
        web_acl.directives.create_before_destroy = true;
        state.upsert_resource(web_acl);

        let mut distribution = ResourceState::new("test.resource", "distribution", "mock")
            .with_identifier("distribution-id")
            .with_attribute("name", serde_json::json!("distribution"))
            .with_attribute("comment", serde_json::json!("before"))
            .with_attribute("web_acl_arn", serde_json::json!("web-acl-old"));
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
                "identifier": "old-web-acl-id"
            },
            "test.resource.distribution": {
                "name": "distribution",
                "comment": "before",
                "web_acl_arn": "web-acl-old"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn seed_rename_state(&self) {
        let mut state = StateFile::new();
        let mut renamed = ResourceState::new("test.renameable_resource", "renamed", "mock")
            .with_identifier("old-rename-id")
            .with_attribute("name", serde_json::json!("rename-target"))
            .with_attribute("force_replace", serde_json::json!("old"));
        renamed.binding = Some("renamed".to_string());
        renamed.directives.create_before_destroy = true;
        state.upsert_resource(renamed);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_rename_mock_provider_state(&self) {
        let provider_state = serde_json::json!({
            "test.renameable_resource.renamed": {
                "name": "rename-target",
                "force_replace": "old"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn seed_rename_consumer_state(&self) {
        let mut state = StateFile::new();
        let mut renamed = ResourceState::new("test.renameable_resource", "renamed", "mock")
            .with_identifier("old-rename-id")
            .with_attribute("name", serde_json::json!("rename-target"))
            .with_attribute("force_replace", serde_json::json!("old"));
        renamed.binding = Some("renamed".to_string());
        renamed.directives.create_before_destroy = true;
        state.upsert_resource(renamed);

        let mut consumer = ResourceState::new("test.resource", "consumer", "mock")
            .with_identifier("consumer-id")
            .with_attribute("name", serde_json::json!("consumer"))
            .with_attribute("comment", serde_json::json!("before"));
        consumer.binding = Some("consumer".to_string());
        consumer.dependency_bindings.insert("renamed".to_string());
        state.upsert_resource(consumer);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_rename_consumer_mock_provider_state(&self) {
        let provider_state = serde_json::json!({
            "test.renameable_resource.renamed": {
                "name": "rename-target",
                "force_replace": "old"
            },
            "test.resource.consumer": {
                "name": "consumer",
                "comment": "before"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn seed_permanent_state(&self) {
        let mut state = StateFile::new();
        let mut permanent = ResourceState::new("test.resource", "permanent", "mock")
            .with_identifier("old-permanent-id")
            .with_attribute("name", serde_json::json!("stable-name"))
            .with_attribute("force_replace", serde_json::json!("old"));
        permanent.binding = Some("permanent".to_string());
        permanent.directives.create_before_destroy = true;
        state.upsert_resource(permanent);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_ref_permanent_state(&self, source_name: &str) {
        let mut state = StateFile::new();
        let mut source = ResourceState::new("test.renameable_resource", "source", "mock")
            .with_identifier("source-id")
            .with_attribute("name", serde_json::json!(source_name));
        source.binding = Some("source".to_string());
        state.upsert_resource(source);

        let mut permanent = ResourceState::new("test.resource", "permanent", "mock")
            .with_identifier("old-permanent-id")
            .with_attribute("name", serde_json::json!(source_name))
            .with_attribute("force_replace", serde_json::json!("old"));
        permanent.binding = Some("permanent".to_string());
        permanent.directives.create_before_destroy = true;
        permanent.dependency_bindings.insert("source".to_string());
        state.upsert_resource(permanent);

        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }

    fn seed_permanent_mock_provider_state(&self, name: &str, force_replace: &str) {
        let provider_state = serde_json::json!({
            "test.resource.permanent": {
                "name": name,
                "force_replace": force_replace
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn seed_ref_permanent_mock_provider_state(&self, source_name: &str, permanent_name: &str) {
        let provider_state = serde_json::json!({
            "test.renameable_resource.source": {
                "name": source_name
            },
            "test.resource.permanent": {
                "name": permanent_name,
                "force_replace": "old"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn seed_mock_provider_state_from_carina_state(&self) {
        let provider_state: serde_json::Map<String, serde_json::Value> = self
            .state()
            .resources
            .into_iter()
            .map(|row| {
                (
                    format!("{}.{}", row.resource_type, row.name),
                    serde_json::Value::Object(row.attributes.into_iter().collect()),
                )
            })
            .collect();
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&serde_json::Value::Object(provider_state))
                .unwrap(),
        )
        .unwrap();
    }

    fn apply(&self) -> Output {
        carina(&self.project)
            .args([
                "apply",
                ".",
                "--auto-approve",
                "--lock=false",
                "--parallelism",
                "8",
            ])
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_OP_LOG", &self.op_log_path)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .env("CARINA_MOCK_UPDATE_DELAY_MS", UPDATE_DELAY_MS)
            .output()
            .expect("failed to execute carina apply")
    }

    fn apply_with_delete_failure(&self, pattern: &str) -> Output {
        carina(&self.project)
            .args([
                "apply",
                ".",
                "--auto-approve",
                "--lock=false",
                "--parallelism",
                "8",
            ])
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_OP_LOG", &self.op_log_path)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .env("CARINA_MOCK_DELETE_FAIL_FOR", pattern)
            .output()
            .expect("failed to execute carina apply")
    }

    fn plan(&self) -> Output {
        carina(&self.project)
            .args(["plan", "."])
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .output()
            .expect("failed to execute carina plan")
    }

    fn state(&self) -> StateFile {
        serde_json::from_str(&fs::read_to_string(&self.state_path).unwrap()).unwrap()
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
fn apply_name_overrides_applies_for_var_substituted_dsl_name() {
    let scenario = Scenario::new();
    scenario.write_interpolated_permanent_name_config("stable");
    scenario.init();
    scenario.seed_permanent_state();
    scenario.seed_permanent_mock_provider_state("stable-name", "old");

    let output = scenario.apply();
    assert_success("carina apply with interpolated name", &output);
    let state = scenario.state();
    let first_override = state
        .find_resource("mock", "test.resource", "permanent")
        .and_then(|row| row.name_overrides.get("name"))
        .expect("first apply should record a permanent name override")
        .clone();
    assert_eq!(first_override.original_value, "stable-name");

    scenario.seed_mock_provider_state_from_carina_state();
    let output = scenario.apply();
    assert_apply_no_changes("second apply with unchanged interpolated name", &output);
}

#[test]
fn apply_name_overrides_skips_for_ref_substituted_dsl_name_rename() {
    let scenario = Scenario::new();
    scenario.write_ref_permanent_name_config("stable-name");
    scenario.init();
    scenario.seed_ref_permanent_state("stable-name");
    scenario.seed_ref_permanent_mock_provider_state("stable-name", "stable-name");

    let output = scenario.apply();
    assert_success("carina apply with ref-valued name", &output);
    let state = scenario.state();
    let first_override = state
        .find_resource("mock", "test.resource", "permanent")
        .and_then(|row| row.name_overrides.get("name"))
        .expect("first apply should record a permanent name override")
        .clone();
    assert_eq!(first_override.original_value, "stable-name");

    scenario.seed_mock_provider_state_from_carina_state();
    scenario.write_ref_permanent_name_config("stable-name-v2");
    let output = scenario.plan();
    assert_success("carina plan after ref-valued DSL rename", &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("to replace") || stdout.contains("+/-"),
        "ref-valued DSL rename must trigger a new CBD replacement, not no-op\nstdout:\n{stdout}"
    );

    let output = scenario.apply();
    assert_success("carina apply after ref-valued DSL rename", &output);
    let state = scenario.state();
    let second_override = state
        .find_resource("mock", "test.resource", "permanent")
        .and_then(|row| row.name_overrides.get("name"))
        .expect("second apply should record a fresh permanent name override");
    assert_eq!(second_override.original_value, "stable-name-v2");
    assert_ne!(second_override.temp_value, first_override.temp_value);

    scenario.seed_mock_provider_state_from_carina_state();
    let output = scenario.plan();
    assert_no_changes("carina plan after ref-valued rename apply", &output);
}

#[test]
fn test_cbd_consumer_reading_name_does_not_break() {
    let scenario = Scenario::new();
    scenario.write_rename_consumer_config();
    scenario.init();
    scenario.seed_rename_consumer_state();
    scenario.seed_rename_consumer_mock_provider_state();

    let output = scenario.apply();
    assert_success("carina apply", &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("cycle detected") && !stderr.contains("cycle detected"),
        "CBD consumer apply must not be skipped by a dependency cycle\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let op_log = scenario.op_log();
    let create_pos = op_log
        .iter()
        .position(|entry| entry == "create test.renameable_resource.renamed")
        .expect("operation log should contain replacement create");
    let consumer_update_pos = op_log
        .iter()
        .position(|entry| entry == "update test.resource.consumer")
        .expect("operation log should contain consumer update");
    let delete_pos = op_log
        .iter()
        .position(|entry| entry == "delete test.renameable_resource.renamed")
        .expect("operation log should contain old resource delete");
    assert!(
        create_pos < consumer_update_pos && consumer_update_pos < delete_pos,
        "consumer update must run between CBD create and delete; observed log: {op_log:?}"
    );
    assert!(
        !op_log
            .iter()
            .any(|entry| entry == "update test.renameable_resource.renamed"),
        "CBD must not issue a rename update; observed log: {op_log:?}"
    );

    scenario.seed_mock_provider_state_from_carina_state();
    let output = scenario.plan();
    assert_no_changes("carina plan after apply", &output);
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
    let create_pos = op_log
        .iter()
        .position(|entry| entry == "create test.resource.web_acl")
        .expect("operation log should contain web_acl create");
    let update_pos = op_log
        .iter()
        .position(|entry| entry == "update test.resource.distribution")
        .expect("operation log should contain distribution update");
    let delete_pos = op_log
        .iter()
        .position(|entry| entry == "delete test.resource.web_acl")
        .expect("operation log should contain web_acl delete");

    assert!(
        create_pos < update_pos && update_pos < delete_pos,
        "#3625: consumer update must run between CBD create and delete; observed log: {op_log:?}"
    );

    scenario.seed_mock_provider_state_from_carina_state();
    let output = scenario.plan();
    assert_no_changes("carina plan after apply", &output);
}

#[test]
fn test_cbd_permanent_name_override_persists() {
    let scenario = Scenario::new();
    scenario.write_permanent_name_config();
    scenario.init();
    scenario.seed_permanent_state();
    scenario.seed_permanent_mock_provider_state("stable-name", "old");

    let output = scenario.apply();
    assert_success("carina apply", &output);

    let state = scenario.state();
    let row = state
        .find_resource("mock", "test.resource", "permanent")
        .expect("permanent resource should remain in state");
    let temporary_name = row
        .name_overrides
        .get("name")
        .expect("CBD temporary name should persist a permanent name override")
        .temp_value
        .clone();
    assert!(
        temporary_name.starts_with("stable-name-"),
        "temporary name should be based on the desired name, got {temporary_name}"
    );

    scenario.seed_permanent_mock_provider_state(&temporary_name, "new");
    let output = scenario.plan();
    assert_success("carina plan", &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No changes. Infrastructure is up-to-date.")
            && !stdout.contains("to replace"),
        "permanent name override should prevent a follow-up replace plan\nstdout:\n{stdout}"
    );
}

#[test]
fn test_cbd_dsl_rename_after_apply_triggers_new_cbd() {
    let scenario = Scenario::new();
    scenario.write_permanent_name_config_with_name("stable-name");
    scenario.init();
    scenario.seed_permanent_state();
    scenario.seed_permanent_mock_provider_state("stable-name", "old");

    let output = scenario.apply();
    assert_success("carina apply", &output);
    let state = scenario.state();
    let first_override = state
        .find_resource("mock", "test.resource", "permanent")
        .and_then(|row| row.name_overrides.get("name"))
        .expect("first apply should record a permanent name override")
        .clone();
    assert_eq!(first_override.original_value, "stable-name");

    scenario.seed_mock_provider_state_from_carina_state();
    scenario.write_permanent_name_config_with_name("stable-name-v2");
    let output = scenario.plan();
    assert_success("carina plan after DSL rename", &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("to replace") || stdout.contains("+/-"),
        "DSL rename must trigger a new CBD replacement, not no-op\nstdout:\n{stdout}"
    );

    let output = scenario.apply();
    assert_success("carina apply after DSL rename", &output);
    let state = scenario.state();
    let second_override = state
        .find_resource("mock", "test.resource", "permanent")
        .and_then(|row| row.name_overrides.get("name"))
        .expect("second apply should record a fresh permanent name override");
    assert_eq!(second_override.original_value, "stable-name-v2");
    assert_ne!(second_override.temp_value, first_override.temp_value);
    assert!(
        second_override.temp_value.starts_with("stable-name-v2-"),
        "new override should be based on the new DSL name, got {}",
        second_override.temp_value
    );

    scenario.seed_mock_provider_state_from_carina_state();
    let output = scenario.plan();
    assert_no_changes("carina plan after second apply", &output);
}

#[test]
fn test_cbd_delete_failure_records_new_with_temp_name_override() {
    let scenario = Scenario::new();
    scenario.write_rename_config();
    scenario.init();
    scenario.seed_rename_state();
    scenario.seed_rename_mock_provider_state();

    let output = scenario.apply_with_delete_failure("test.renameable_resource.renamed");
    assert!(
        !output.status.success(),
        "apply must fail when old CBD delete fails\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("create_before_destroy replacement")
            && stderr.contains("old resource delete did not complete")
            && stderr.contains("State now records the replacement"),
        "delete failure should warn about the recorded replacement\nstderr:\n{stderr}"
    );

    let state = scenario.state();
    let row = state
        .find_resource("mock", "test.renameable_resource", "renamed")
        .expect("renameable temp-name replacement should be recorded in state");
    assert_eq!(row.identifier.as_deref(), Some("mock-id"));
    let temporary_name = row
        .attributes
        .get("name")
        .and_then(serde_json::Value::as_str)
        .expect("created replacement should keep its temporary name");
    assert!(
        temporary_name.starts_with("rename-target-"),
        "temporary name should be based on the desired name, got {temporary_name}"
    );
    assert_eq!(
        row.attributes.get("force_replace"),
        Some(&serde_json::json!("new")),
        "CBD should record the created replacement even when old delete fails"
    );
    assert_eq!(
        row.name_overrides
            .get("name")
            .map(|override_| override_.temp_value.as_str()),
        Some(temporary_name),
        "CBD temporary names should become permanent overrides"
    );

    let op_log = scenario.op_log();
    assert!(
        op_log
            .iter()
            .any(|entry| entry == "create test.renameable_resource.renamed")
            && op_log
                .iter()
                .any(|entry| entry == "delete-fail test.renameable_resource.renamed")
            && !op_log
                .iter()
                .any(|entry| entry == "update test.renameable_resource.renamed"),
        "no rename update should run after failed delete; observed log: {op_log:?}"
    );
}

#[test]
fn test_cbd_permanent_temp_name_delete_failure_records_new() {
    let scenario = Scenario::new();
    scenario.write_permanent_name_config();
    scenario.init();
    scenario.seed_permanent_state();
    scenario.seed_permanent_mock_provider_state("stable-name", "old");

    let output = scenario.apply_with_delete_failure("test.resource.permanent");
    assert!(
        !output.status.success(),
        "apply must fail when old CBD delete fails\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let state = scenario.state();
    let row = state
        .find_resource("mock", "test.resource", "permanent")
        .expect("permanent temp-name replacement should be recorded in state");
    assert_eq!(row.identifier.as_deref(), Some("mock-id"));
    assert_eq!(
        row.attributes.get("force_replace"),
        Some(&serde_json::json!("new")),
        "CBD should record the created replacement even when old delete fails"
    );
    let temporary_name = row
        .name_overrides
        .get("name")
        .expect("permanent temp-name replacement should persist name override");
    assert!(
        temporary_name.temp_value.starts_with("stable-name-"),
        "temporary name should be based on the desired name, got {}",
        temporary_name.temp_value
    );

    let op_log = scenario.op_log();
    assert!(
        op_log
            .iter()
            .any(|entry| entry == "create test.resource.permanent")
            && op_log
                .iter()
                .any(|entry| entry == "delete-fail test.resource.permanent"),
        "create should be recorded before the failed delete; observed log: {op_log:?}"
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

fn assert_no_changes(label: &str, output: &Output) {
    assert_success(label, output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No changes. Infrastructure is up-to-date."),
        "{label} should report no changes\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_apply_no_changes(label: &str, output: &Output) {
    assert_success(label, output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No changes needed."),
        "{label} should report no changes\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}
