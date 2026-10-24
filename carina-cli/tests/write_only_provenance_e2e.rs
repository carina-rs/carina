use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_core::resource::{ConcreteValue, Value};
use carina_state::check_and_migrate;
use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temporary project");
        let project = tmp.path().to_path_buf();
        Self {
            mock_state: project.join("mock-state.json"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self, password: Option<&str>) {
        let password = password
            .map(|password| format!("  password = \"{password}\"\n"))
            .unwrap_or_default();
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let r1 = mock.test.resource {{
  name = "r1"
{password}}}
"#
            ),
        )
        .expect("write configuration");
    }

    fn write_secret_config(&self, password: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let r1 = mock.test.resource {{
  name     = "r1"
  password = secret("{password}")
}}
"#
            ),
        )
        .expect("write secret configuration");
    }

    fn clear_write_only_metadata(&self) {
        let path = self.project.join("carina.state.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read state JSON"))
                .expect("parse state JSON");
        let row = state["resources"]
            .as_array_mut()
            .expect("state resources")
            .iter_mut()
            .find(|row| row["identity"] == "r1")
            .expect("r1 state row");
        row["current"]
            .as_object_mut()
            .expect("current instance")
            .remove("write_only_attributes");
        fs::write(
            path,
            serde_json::to_vec_pretty(&state).expect("serialize state JSON"),
        )
        .expect("write legacy-shaped state JSON");
    }

    fn init(&self) -> Output {
        carina(&self.project)
            .args(["init", "."])
            .output()
            .expect("run carina init")
    }

    fn save_plan(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["plan", ".", "--refresh=false", "--out", name])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("save carina plan")
    }

    fn save_plan_with_refresh(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["plan", ".", "--out", name])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("save carina plan with refresh")
    }

    fn apply_plan(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["apply", name, "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("apply saved plan")
    }

    fn apply_plan_with_failed_update(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["apply", name, "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_UPDATE_FAIL_FOR", "test.resource.r1")
            .output()
            .expect("apply saved plan with failed update")
    }

    fn plan(&self) -> Output {
        carina(&self.project)
            .args(["plan", ".", "--refresh=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("run carina plan")
    }

    fn plan_with_refresh(&self) -> Output {
        carina(&self.project)
            .args(["plan", "."])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("run carina plan with refresh")
    }
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

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn failed_update_does_not_acknowledge_write_only_value_and_next_plan_retries_it() {
    let scenario = Scenario::new();
    scenario.write_config(Some("old"));
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.write_config(Some("new"));
    assert_success("update plan", &scenario.save_plan("update.json"));
    let failed = scenario.apply_plan_with_failed_update("update.json");
    let failed_text = output_text(&failed);
    assert!(
        !failed.status.success(),
        "mock update must fail\nstdout/stderr:\n{failed_text}"
    );
    assert!(
        failed_text.contains("requested update failure"),
        "apply must report the injected update failure\nstdout/stderr:\n{failed_text}"
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json"))
        .expect("read state after failed update");
    let state = check_and_migrate(&state_text)
        .expect("state after failed update must decode")
        .into_state();
    let password = state
        .find_resource("mock", "test.resource", "r1")
        .and_then(|row| row.persisted_attributes())
        .and_then(|attributes| attributes.get("password"));
    assert_eq!(
        password,
        Some(Value::Concrete(ConcreteValue::String("old".into()))),
        "failed update must keep the last accepted write-only value"
    );

    let plan = scenario.plan();
    assert_success("plan after failed update", &plan);
    let plan_text = output_text(&plan);
    assert!(
        plan_text.contains("~ mock.test.resource r1") && plan_text.contains("1 to change"),
        "next plan must retry the write-only change\nstdout/stderr:\n{plan_text}"
    );
}

#[test]
fn adding_and_changing_updatable_write_only_value_converges() {
    let scenario = Scenario::new();
    scenario.write_config(None);
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.write_config(Some("a"));
    let add_plan = scenario.save_plan("add-password.json");
    assert_success("add-password plan", &add_plan);
    let add_plan_text = output_text(&add_plan);
    assert!(
        add_plan_text.contains("~ mock.test.resource r1") && add_plan_text.contains("1 to change"),
        "adding an updatable write-only value must plan an update\nstdout/stderr:\n{add_plan_text}"
    );
    assert_success(
        "add-password apply",
        &scenario.apply_plan("add-password.json"),
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json"))
        .expect("read state after adding password");
    let state = check_and_migrate(&state_text)
        .expect("state after adding password must decode")
        .into_state();
    assert_eq!(
        state
            .find_resource("mock", "test.resource", "r1")
            .and_then(|row| row.persisted_attributes())
            .and_then(|attributes| attributes.get("password")),
        Some(Value::Concrete(ConcreteValue::String("a".into())))
    );

    scenario.write_config(Some("b"));
    let change_plan = scenario.save_plan("change-password.json");
    assert_success("change-password plan", &change_plan);
    let change_plan_text = output_text(&change_plan);
    assert!(
        change_plan_text.contains("~ mock.test.resource r1")
            && change_plan_text.contains("1 to change"),
        "changing an accepted write-only value must plan another update\nstdout/stderr:\n{change_plan_text}"
    );
    assert_success(
        "change-password apply",
        &scenario.apply_plan("change-password.json"),
    );

    let unchanged = scenario.plan();
    assert_success("unchanged plan", &unchanged);
    let unchanged_text = output_text(&unchanged);
    assert!(
        unchanged_text.contains("No changes. Infrastructure is up-to-date."),
        "accepted write-only update must converge\nstdout/stderr:\n{unchanged_text}"
    );
}

#[test]
fn refresh_uses_persisted_write_only_value_when_provider_read_omits_it() {
    let scenario = Scenario::new();
    scenario.write_config(Some("a"));
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    let unchanged = scenario.plan_with_refresh();
    assert_success("refresh-on unchanged plan", &unchanged);
    let unchanged_text = output_text(&unchanged);
    assert!(
        unchanged_text.contains("No changes. Infrastructure is up-to-date."),
        "refresh must restore the persisted last-sent password when read omits it\nstdout/stderr:\n{unchanged_text}"
    );

    scenario.write_config(Some("b"));
    let changed = scenario.plan_with_refresh();
    assert_success("refresh-on changed plan", &changed);
    let changed_text = output_text(&changed);
    assert!(
        changed_text.contains("~ mock.test.resource r1") && changed_text.contains("1 to change"),
        "changing the password after refresh must still plan an update\nstdout/stderr:\n{changed_text}"
    );
}

#[test]
fn refresh_and_saved_plan_drift_use_legacy_unlisted_write_only_value() {
    let scenario = Scenario::new();
    scenario.write_config(Some("legacy-value"));
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));
    scenario.clear_write_only_metadata();

    let refreshed = scenario.plan_with_refresh();
    assert_success("legacy-shaped refresh-on plan", &refreshed);
    let refreshed_text = output_text(&refreshed);
    assert!(
        refreshed_text.contains("No changes. Infrastructure is up-to-date."),
        "schema write-only values present in persisted attributes must not require legacy metadata\nstdout/stderr:\n{refreshed_text}"
    );

    assert_success(
        "legacy-shaped saved plan",
        &scenario.save_plan("unchanged.json"),
    );
    let applied = scenario.apply_plan("unchanged.json");
    assert_success("legacy-shaped saved-plan apply", &applied);
    assert!(
        !output_text(&applied).contains("Infrastructure drift detected"),
        "saved-plan apply must not report false drift for a provider-omitted write-only value"
    );
}

#[test]
fn secret_write_only_refresh_and_saved_plan_apply_converge_without_sending_hash() {
    let scenario = Scenario::new();
    scenario.write_secret_config("secret-before");
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    let state_path = scenario.project.join("carina.state.json");
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&state_path).expect("read secret state after create"),
    )
    .expect("parse secret state after create");
    let persisted_password = &state["resources"][0]["current"]["attributes"]["password"];
    assert!(
        persisted_password.get("$secret").is_some(),
        "write-only secret must be persisted as a RedactedSecret hash: {state:#}"
    );
    scenario.clear_write_only_metadata();

    assert_success(
        "unchanged secret saved plan",
        &scenario.save_plan("unchanged-secret.json"),
    );
    let unchanged_apply = scenario.apply_plan("unchanged-secret.json");
    assert_success("unchanged secret saved-plan apply", &unchanged_apply);
    assert!(
        !output_text(&unchanged_apply).contains("Infrastructure drift detected"),
        "saved-plan apply must not report false secret drift"
    );

    let unchanged = scenario.plan_with_refresh();
    assert_success("unchanged secret refresh-on plan", &unchanged);
    let unchanged_text = output_text(&unchanged);
    assert!(
        unchanged_text.contains("No changes. Infrastructure is up-to-date."),
        "an unchanged write-only secret must converge after refresh\nstdout/stderr:\n{unchanged_text}"
    );

    scenario.write_secret_config("secret-after");
    let changed = scenario.save_plan_with_refresh("changed-secret.json");
    assert_success("changed secret refresh-on plan", &changed);
    let changed_text = output_text(&changed);
    assert!(
        changed_text.contains("~ mock.test.resource r1") && changed_text.contains("1 to change"),
        "a changed write-only secret must produce an Update\nstdout/stderr:\n{changed_text}"
    );
    assert_success(
        "changed secret saved-plan apply",
        &scenario.apply_plan("changed-secret.json"),
    );

    let provider: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&scenario.mock_state).expect("read mock provider state"),
    )
    .expect("parse mock provider state");
    assert_eq!(
        provider["states"]["test.resource.r1"]["password"],
        serde_json::json!("secret-after"),
        "the mock applies the request patch directly, so a structural hash here would mean the hash reached the provider patch"
    );

    let converged = scenario.plan_with_refresh();
    assert_success("changed secret converged plan", &converged);
    let converged_text = output_text(&converged);
    assert!(
        converged_text.contains("No changes. Infrastructure is up-to-date."),
        "the accepted secret update must converge\nstdout/stderr:\n{converged_text}"
    );
}
