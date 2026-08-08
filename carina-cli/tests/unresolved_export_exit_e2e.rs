//! Process-level regressions for unresolved export writeback (carina#3710).
//!
//! The mock schema declares `web_acl_arn`, but the provider returns no value
//! unless it was configured. Export resolution must therefore preserve state
//! first and then make apply/refresh fail instead of printing a success lie.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let project = temp.path().to_path_buf();
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        scenario.write_base_config();
        scenario
    }

    fn write_base_config(&self) {
        fs::write(
            self.project.join("backend.crn"),
            r#"backend local {
  path = "carina.state.json"
}
"#,
        )
        .expect("backend fixture");
        fs::write(self.project.join("providers.crn"), "provider mock { }\n")
            .expect("provider fixture");
        fs::write(
            self.project.join("main.crn"),
            r#"let role = mock.test.resource {
  name = "role"
}
"#,
        )
        .expect("resource fixture");
        self.write_exports(false);
    }

    fn write_exports(&self, unresolved: bool) {
        let body = if unresolved {
            r#"exports {
  role_arn = role.web_acl_arn
}
"#
        } else {
            "exports { }\n"
        };
        fs::write(self.project.join("exports.crn"), body).expect("exports fixture");
    }

    fn add_failing_resource(&self) {
        fs::write(
            self.project.join("main.crn"),
            r#"let role = mock.test.resource {
  name = "role"
}

let broken = mock.test.resource {
  name = "broken"
}
"#,
        )
        .expect("resource fixture with failing sibling");
    }

    fn init(&self) -> Output {
        self.command()
            .args(["init", "."])
            .output()
            .expect("carina init")
    }

    fn apply(&self) -> Output {
        self.command()
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .output()
            .expect("carina apply")
    }

    fn apply_with_create_failure(&self) -> Output {
        self.command()
            .env("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.broken")
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .output()
            .expect("carina apply with resource failure")
    }

    fn apply_with_partial_create(&self) -> Output {
        self.command()
            .env("CARINA_MOCK_PARTIAL_CREATE_FOR", "*")
            .env("CARINA_MOCK_PARTIAL_CREATE_MISSING", "web_acl_arn")
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .output()
            .expect("carina apply with partial resource create")
    }

    fn refresh(&self) -> Output {
        self.command()
            .args(["state", "refresh", "."])
            .output()
            .expect("carina state refresh")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE");
        command
    }

    fn persisted_state(&self) -> Value {
        let text = fs::read_to_string(self.project.join("carina.state.json"))
            .expect("state must be persisted");
        serde_json::from_str(&text).expect("valid state JSON")
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout/stderr:\n{}",
        output_text(output),
    );
}

fn serial(state: &Value) -> u64 {
    state
        .get("serial")
        .and_then(Value::as_u64)
        .expect("state serial")
}

fn assert_unresolved_export_failure(output: &Output, expected_exit_code: i32) {
    let text = output_text(output);
    assert_eq!(
        output.status.code(),
        Some(expected_exit_code),
        "unresolved export writeback must use the expected exit code\nstdout/stderr:\n{text}",
    );
    assert!(
        text.contains("! export role_arn not written"),
        "per-export diagnostic must be printed\nstdout/stderr:\n{text}",
    );
    assert!(
        text.contains("State saved"),
        "state must be saved before the command returns the export error\nstdout/stderr:\n{text}",
    );
    assert!(
        text.contains("1 export not written to state: role_arn"),
        "the final command error must report the skipped count and export name\nstdout/stderr:\n{text}",
    );
}

fn assert_apply_export_failure(output: &Output, expected_exit_code: i32) {
    assert_unresolved_export_failure(output, expected_exit_code);
    let text = output_text(output);
    assert!(
        !text.contains("Apply complete!"),
        "a skipped export must not print the apply success summary\nstdout/stderr:\n{text}",
    );
}

fn assert_exports_only_failure(output: &Output) {
    assert_unresolved_export_failure(output, 1);
    let text = output_text(output);
    assert!(
        !text.contains("Exports updated"),
        "a skipped export must not print the export success summary\nstdout/stderr:\n{text}",
    );
}

fn assert_refresh_failure(output: &Output) {
    assert_unresolved_export_failure(output, 1);
}

#[test]
fn mutating_apply_saves_resource_then_fails_for_unresolved_export() {
    let scenario = Scenario::new();
    scenario.write_exports(true);
    assert_success("carina init", &scenario.init());

    let apply = scenario.apply();

    assert_apply_export_failure(&apply, 1);
    let text = output_text(&apply);
    assert!(
        text.contains("Apply failed. 1 succeeded, 1 export not written to state: role_arn"),
        "pure export failure must use the same success-count summary as mixed failures\nstdout/stderr:\n{text}",
    );
    let state = scenario.persisted_state();
    assert!(
        state.to_string().contains("mock-id"),
        "the successfully-created resource must remain persisted: {state:#}",
    );
}

#[test]
fn resource_failure_message_also_reports_unresolved_export() {
    let scenario = Scenario::new();
    scenario.add_failing_resource();
    scenario.write_exports(true);
    assert_success("carina init", &scenario.init());

    let apply = scenario.apply_with_create_failure();

    assert_apply_export_failure(&apply, 1);
    let text = output_text(&apply);
    assert!(
        text.contains("1 failed") && text.contains("1 export not written"),
        "export failure must be appended without masking resource failure\nstdout/stderr:\n{text}",
    );
}

#[test]
fn partial_apply_with_unresolved_export_preserves_exit_two() {
    let scenario = Scenario::new();
    scenario.write_exports(true);
    assert_success("carina init", &scenario.init());

    let apply = scenario.apply_with_partial_create();

    assert_apply_export_failure(&apply, 2);
    let text = output_text(&apply);
    assert!(
        text.contains("1 partial") && text.contains("1 export not written"),
        "partial resource and export failure must both be reported\nstdout/stderr:\n{text}",
    );
}

#[test]
fn exports_only_apply_saves_state_then_fails_for_unresolved_export() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.init());
    assert_success("initial carina apply", &scenario.apply());
    let before = scenario.persisted_state();
    scenario.write_exports(true);

    let apply = scenario.apply();

    assert_exports_only_failure(&apply);
    let after = scenario.persisted_state();
    assert!(
        serial(&after) > serial(&before),
        "exports-only writeback must save state before failing: before={before:#}, after={after:#}",
    );
}

#[test]
fn state_refresh_saves_state_then_fails_for_unresolved_export() {
    let scenario = Scenario::new();
    assert_success("carina init", &scenario.init());
    assert_success("initial carina apply", &scenario.apply());
    let before = scenario.persisted_state();
    scenario.write_exports(true);

    let refresh = scenario.refresh();

    assert_refresh_failure(&refresh);
    let refresh_text = output_text(&refresh);
    assert!(
        refresh_text.contains("Lock acquired"),
        "refresh must exercise the default locked save path\nstdout/stderr:\n{refresh_text}",
    );
    let after = scenario.persisted_state();
    assert!(
        serial(&after) > serial(&before),
        "refresh must save state before failing: before={before:#}, after={after:#}",
    );
}
