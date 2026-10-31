use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::resource::{ConcreteValue, Value};
use carina_state::check_and_migrate;
use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    update_patch_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temporary project");
        let project = tmp.path().to_path_buf();
        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend configuration");
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        Self {
            mock_state: project.join("mock-state.json"),
            update_patch_log: project.join("update-patches.jsonl"),
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
                r#"let r1 = mock.test.resource {{
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
                r#"let r1 = mock.test.resource {{
  name     = "r1"
  password = secret("{password}")
}}
"#
            ),
        )
        .expect("write secret configuration");
    }

    fn write_secret_and_comment_config(&self, password: &str, comment: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"let r1 = mock.test.resource {{
  name     = "r1"
  password = secret("{password}")
  comment  = "{comment}"
}}
"#
            ),
        )
        .expect("write secret and comment configuration");
    }

    fn write_nested_secret_and_comment_config(&self, client_secret: &str, comment: &str) {
        self.write_nested_secret_and_comment_config_with_ignore(client_secret, comment, false);
    }

    fn write_nested_secret_and_comment_config_with_ignore(
        &self,
        client_secret: &str,
        comment: &str,
        ignore_client_secret: bool,
    ) {
        let directives = if ignore_client_secret {
            r#"
  directives {
    ignore_changes = [oidc_config.client_secret]
  }
"#
        } else {
            ""
        };
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"let r1 = mock.test.resource {{
  name    = "r1"
  comment = "{comment}"
  oidc_config = {{
    issuer        = "https://issuer.example.com"
    client_secret = secret("{client_secret}")
  }}
{directives}
}}
"#
            ),
        )
        .expect("write nested secret and comment configuration");
    }

    fn write_readable_ignore_config(&self, comment: &str, owner: Option<&str>) {
        let tags = owner
            .map(|owner| {
                format!(
                    r#"  tags = {{
    Owner = "{owner}"
  }}
"#
                )
            })
            .unwrap_or_default();
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"let r1 = mock.test.resource {{
  name    = "r1"
  comment = "{comment}"
{tags}
  directives {{
    ignore_changes = [comment]
  }}
}}
"#
            ),
        )
        .expect("write readable ignore_changes configuration");
    }

    fn mutate_provider_attribute(&self, attribute: &str, value: serde_json::Value) {
        let mut provider: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&self.mock_state).expect("read mock provider state"),
        )
        .expect("parse mock provider state");
        provider["states"]["test.resource.r1"][attribute] = value;
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&provider).expect("serialize mock provider state"),
        )
        .expect("write externally changed mock provider state");
    }

    fn mutate_provider_nested_client_secret(&self, value: &str) {
        self.mutate_provider_attribute(
            "oidc_config",
            serde_json::json!({
                "issuer": "https://issuer.example.com",
                "client_secret": value,
            }),
        );
    }

    fn provider_state(&self) -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(&self.mock_state).expect("read mock provider state"),
        )
        .expect("parse mock provider state")
    }

    fn update_patch_operations(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(&self.update_patch_log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse mock update patch log entry"))
            .collect()
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
            .env("CARINA_MOCK_UPDATE_PATCH_LOG", &self.update_patch_log)
            .output()
            .expect("save carina plan")
    }

    fn save_plan_with_refresh(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["plan", ".", "--out", name])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_UPDATE_PATCH_LOG", &self.update_patch_log)
            .output()
            .expect("save carina plan with refresh")
    }

    fn apply_plan(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["apply", name, "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_UPDATE_PATCH_LOG", &self.update_patch_log)
            .output()
            .expect("apply saved plan")
    }

    fn apply_plan_with_failed_update(&self, name: &str) -> Output {
        carina(&self.project)
            .args(["apply", name, "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_UPDATE_PATCH_LOG", &self.update_patch_log)
            .env("CARINA_MOCK_UPDATE_FAIL_FOR", "test.resource.r1")
            .output()
            .expect("apply saved plan with failed update")
    }

    fn plan(&self) -> Output {
        carina(&self.project)
            .args(["plan", ".", "--refresh=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_UPDATE_PATCH_LOG", &self.update_patch_log)
            .output()
            .expect("run carina plan")
    }

    fn plan_with_refresh(&self) -> Output {
        carina(&self.project)
            .args(["plan", "."])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_UPDATE_PATCH_LOG", &self.update_patch_log)
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

fn tamper_authored_ignore_paths(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => {
            values.iter_mut().map(tamper_authored_ignore_paths).sum()
        }
        serde_json::Value::Object(fields) => {
            let mut tampered = 0;
            if let Some(paths) = fields
                .get_mut("ignore_changes")
                .and_then(|value| value.as_array_mut())
            {
                for path in paths {
                    if let Some(root) = path["segments"]
                        .as_array_mut()
                        .and_then(|segments| segments.first_mut())
                    {
                        root["value"] = serde_json::json!("does_not_exist");
                        tampered += 1;
                    }
                }
            }
            tampered
                + fields
                    .values_mut()
                    .map(tamper_authored_ignore_paths)
                    .sum::<usize>()
        }
        _ => 0,
    }
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
fn normal_update_reasserts_unchanged_write_only_value_and_converges() {
    let scenario = Scenario::new();
    scenario.write_secret_and_comment_config("reasserted-secret", "before");
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.write_secret_and_comment_config("reasserted-secret", "after");
    let update_plan = scenario.save_plan("update-comment.json");
    assert_success("normal-attribute update plan", &update_plan);
    let update_plan_text = output_text(&update_plan);
    assert!(
        update_plan_text.contains("~ mock.test.resource r1")
            && update_plan_text.contains("1 to change"),
        "changing only the normal attribute must plan an update\nstdout/stderr:\n{update_plan_text}"
    );
    assert_success(
        "normal-attribute update apply",
        &scenario.apply_plan("update-comment.json"),
    );

    let provider: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&scenario.mock_state).expect("read mock provider state"),
    )
    .expect("parse mock provider state");
    // This pins Reassert: the mock applies the patch over `from` and omits `password` on read, so plaintext can only arrive through the patch.
    assert_eq!(
        provider["states"]["test.resource.r1"]["password"],
        serde_json::json!("reasserted-secret"),
        "the unchanged write-only secret must be reasserted as provider plaintext, not copied from the redacted current state"
    );
    assert_eq!(
        provider["states"]["test.resource.r1"]["comment"],
        serde_json::json!("after")
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json"))
        .expect("read state after reasserting password");
    let state = check_and_migrate(&state_text)
        .expect("state after reasserting password must decode")
        .into_state();
    let row = state
        .find_resource("mock", "test.resource", "r1")
        .expect("r1 state row");
    assert_eq!(
        row.write_only_attributes(),
        ["password"],
        "the write-only row must keep password recorded after the update"
    );

    let converged = scenario.plan_with_refresh();
    assert_success("plan after reassert update", &converged);
    let converged_text = output_text(&converged);
    assert!(
        converged_text.contains("No changes. Infrastructure is up-to-date."),
        "the normal update plus write-only reassert must converge\nstdout/stderr:\n{converged_text}"
    );
}

#[test]
fn nested_write_only_secret_rotates_reasserts_and_converges() {
    let scenario = Scenario::new();
    scenario.write_nested_secret_and_comment_config("secret-a", "before");
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    let state_path = scenario.project.join("carina.state.json");
    let state_text = fs::read_to_string(&state_path).expect("read state after create");
    let state_json: serde_json::Value =
        serde_json::from_str(&state_text).expect("parse state after create");
    let nested_record = &state_json["resources"][0]["current"]["nested_write_only"];
    assert!(nested_record["oidc_config"].is_array(), "{state_json:#}");
    assert!(
        nested_record.to_string().contains("$secret")
            && !nested_record.to_string().contains("secret-a"),
        "nested secret provenance must contain only a hash: {state_json:#}"
    );

    let unchanged = scenario.plan_with_refresh();
    assert_success("unchanged nested-secret plan", &unchanged);
    assert!(
        output_text(&unchanged).contains("No changes. Infrastructure is up-to-date."),
        "provider omission must converge once leaf provenance exists\n{}",
        output_text(&unchanged)
    );

    scenario.write_nested_secret_and_comment_config("secret-b", "before");
    let rotation = scenario.save_plan_with_refresh("rotate.json");
    assert_success("nested-secret rotation plan", &rotation);
    assert!(
        output_text(&rotation).contains("~ mock.test.resource r1")
            && output_text(&rotation).contains("1 to change"),
        "rotating the nested secret must plan an update\n{}",
        output_text(&rotation)
    );
    assert_success(
        "nested-secret rotation apply",
        &scenario.apply_plan("rotate.json"),
    );

    scenario.write_nested_secret_and_comment_config("secret-b", "after");
    assert_success(
        "unrelated update plan",
        &scenario.save_plan_with_refresh("comment.json"),
    );
    assert_success(
        "unrelated update apply",
        &scenario.apply_plan("comment.json"),
    );

    let provider: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&scenario.mock_state).expect("read mock provider state"),
    )
    .expect("parse mock provider state");
    assert_eq!(
        provider["states"]["test.resource.r1"]["oidc_config"]["client_secret"],
        serde_json::json!("secret-b"),
        "an unrelated update must reassert the whole write-only-bearing attribute"
    );
    assert_eq!(
        provider["states"]["test.resource.r1"]["comment"],
        serde_json::json!("after")
    );

    let converged = scenario.plan_with_refresh();
    assert_success("nested-secret converged plan", &converged);
    assert!(
        output_text(&converged).contains("No changes. Infrastructure is up-to-date."),
        "accepted rotation and reassertion must converge\n{}",
        output_text(&converged)
    );
}

#[test]
fn saved_plan_apply_display_hides_unrelated_nested_write_only_reassert() {
    let scenario = Scenario::new();
    scenario.write_nested_secret_and_comment_config("accepted-secret", "before");
    assert_success("carina init", &scenario.init());
    assert_success("initial plan", &scenario.save_plan("initial.json"));
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.write_nested_secret_and_comment_config("accepted-secret", "after");
    let planned = scenario.save_plan_with_refresh("comment.json");
    assert_success("unrelated nested-secret update plan", &planned);
    let planned_text = output_text(&planned);
    assert!(
        planned_text.contains("comment")
            && !planned_text.contains("oidc_config")
            && !planned_text.contains("client_secret"),
        "ordinary plan display must show only the unrelated change:\n{planned_text}"
    );

    let applied = scenario.apply_plan("comment.json");
    assert_success("unrelated nested-secret saved-plan apply", &applied);
    let applied_text = output_text(&applied);
    assert!(
        applied_text.contains("comment")
            && !applied_text.contains("oidc_config")
            && !applied_text.contains("client_secret"),
        "saved-plan apply display must agree with plan and hide the nested Reassert:\n{applied_text}"
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

#[test]
fn nested_write_only_without_ignore_changes_reasserts_and_rolls_back_external_value() {
    let scenario = Scenario::new();
    scenario.write_nested_secret_and_comment_config_with_ignore("infra-recorded", "before", false);
    assert_success("carina init", &scenario.init());
    assert_success(
        "initial plan",
        &scenario.save_plan_with_refresh("initial.json"),
    );
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.mutate_provider_nested_client_secret("externally-deployed");
    scenario.write_nested_secret_and_comment_config_with_ignore("infra-recorded", "after", false);
    assert_success(
        "unignored update plan",
        &scenario.save_plan_with_refresh("update.json"),
    );
    assert_success(
        "unignored update apply",
        &scenario.apply_plan("update.json"),
    );

    assert_eq!(
        scenario.provider_state()["states"]["test.resource.r1"]["oidc_config"]["client_secret"],
        serde_json::json!("infra-recorded"),
        "the existing Reassert behavior must pin the rollback that ignore_changes prevents"
    );
    let patches = scenario.update_patch_operations();
    let operations = patches
        .last()
        .and_then(|entry| entry["operations"].as_array())
        .expect("update patch operations");
    assert!(
        operations
            .iter()
            .any(|op| op["key"] == "oidc_config" && op["op"] == "reassert"),
        "the unignored update must reassert oidc_config: {patches:#?}"
    );
}

#[test]
fn nested_write_only_ignore_changes_preserves_external_value_and_omits_parent_patch() {
    let scenario = Scenario::new();
    scenario.write_nested_secret_and_comment_config_with_ignore("infra-recorded", "before", true);
    assert_success("carina init", &scenario.init());
    assert_success(
        "initial plan",
        &scenario.save_plan_with_refresh("initial.json"),
    );
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    let state_path = scenario.project.join("carina.state.json");
    let before_state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state after create"))
            .expect("parse state after create");
    let accepted_at_create =
        before_state["resources"][0]["current"]["attributes"]["oidc_config"].clone();
    assert!(
        !before_state.to_string().contains("ignore_changes"),
        "state rows must use the persistence projection without host-only ignore paths: {before_state:#}"
    );

    scenario.mutate_provider_nested_client_secret("externally-deployed");
    scenario.write_nested_secret_and_comment_config_with_ignore("infra-recorded", "after", true);
    assert_success(
        "ignored update plan",
        &scenario.save_plan_with_refresh("update.json"),
    );
    assert_success("ignored update apply", &scenario.apply_plan("update.json"));

    assert_eq!(
        scenario.provider_state()["states"]["test.resource.r1"]["oidc_config"]["client_secret"],
        serde_json::json!("externally-deployed"),
        "omitting the write-only-bearing parent must preserve the external deployment"
    );
    let patches = scenario.update_patch_operations();
    let operations = patches
        .last()
        .and_then(|entry| entry["operations"].as_array())
        .expect("update patch operations");
    assert!(
        operations.iter().any(|op| op["key"] == "comment")
            && operations.iter().all(|op| op["key"] != "oidc_config"),
        "the provider update must contain the unrelated field and omit oidc_config: {patches:#?}"
    );

    let after_state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&state_path).expect("read state after ignored update"),
    )
    .expect("parse state after ignored update");
    assert_eq!(
        after_state["resources"][0]["current"]["attributes"]["oidc_config"], accepted_at_create,
        "ignored write-only leaves must keep their create-time accepted record"
    );
    assert!(!after_state.to_string().contains("ignore_changes"));
}

#[test]
fn readable_ignore_changes_hides_drift_and_omits_it_from_an_unrelated_update() {
    let scenario = Scenario::new();
    scenario.write_readable_ignore_config("configured", Some("old"));
    assert_success("carina init", &scenario.init());
    assert_success(
        "initial plan",
        &scenario.save_plan_with_refresh("initial.json"),
    );
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.mutate_provider_attribute("comment", serde_json::json!("external"));
    let drift = scenario.plan_with_refresh();
    assert_success("ignored readable drift plan", &drift);
    let drift_text = output_text(&drift);
    assert!(
        drift_text.contains("# 1 ignored attribute(s) differ")
            && !drift_text.contains("'external' → 'configured'")
            && !drift_text.contains("~ comment"),
        "ignored readable drift must be summarized without a change row:\n{drift_text}"
    );

    scenario.write_readable_ignore_config("configured", Some("new"));
    assert_success(
        "unrelated readable-ignore update plan",
        &scenario.save_plan_with_refresh("update.json"),
    );
    assert_success(
        "unrelated readable-ignore update apply",
        &scenario.apply_plan("update.json"),
    );
    assert_eq!(
        scenario.provider_state()["states"]["test.resource.r1"]["comment"],
        serde_json::json!("external")
    );
    let patches = scenario.update_patch_operations();
    let operations = patches
        .last()
        .and_then(|entry| entry["operations"].as_array())
        .expect("update patch operations");
    assert!(
        operations.iter().any(|op| op["key"] == "tags")
            && operations.iter().all(|op| op["key"] != "comment"),
        "the unrelated patch must not send the ignored readable attribute: {patches:#?}"
    );
}

#[test]
fn saved_plan_apply_refills_ignored_readable_leaf_from_apply_time_read() {
    let scenario = Scenario::new();
    scenario.write_readable_ignore_config("configured", Some("old"));
    assert_success("carina init", &scenario.init());
    assert_success(
        "initial plan",
        &scenario.save_plan_with_refresh("initial.json"),
    );
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.write_readable_ignore_config("configured", Some("new"));
    assert_success(
        "saved update plan",
        &scenario.save_plan_with_refresh("update.json"),
    );
    let saved: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("update.json")).expect("read saved plan"),
    )
    .expect("parse saved plan");
    assert_eq!(
        saved["version"],
        serde_json::json!(PlanFile::CURRENT_VERSION)
    );
    assert!(
        !saved.to_string().contains("resolved_ignore_changes"),
        "saved plans must not persist schema-resolved ignore metadata: {saved:#}"
    );
    assert_eq!(
        saved["sorted_resources"][0]["directives"]["ignore_changes"][0]["segments"][0]["value"],
        serde_json::json!("comment"),
        "saved plans must retain the authored ignore path: {saved:#}"
    );

    scenario.mutate_provider_attribute("comment", serde_json::json!("between-plan-and-apply"));
    let apply = scenario.apply_plan("update.json");
    assert_success("saved ignored-readable apply", &apply);
    assert!(
        !output_text(&apply).contains("Infrastructure drift detected"),
        "ignored drift between plan and apply must be accepted:\n{}",
        output_text(&apply)
    );
    assert_eq!(
        scenario.provider_state()["states"]["test.resource.r1"]["comment"],
        serde_json::json!("between-plan-and-apply"),
        "apply must refill the sent value from its fresh read"
    );
}

#[test]
fn saved_plan_apply_re_resolves_tampered_authored_ignore_paths() {
    let scenario = Scenario::new();
    scenario.write_readable_ignore_config("configured", Some("old"));
    assert_success("carina init", &scenario.init());
    assert_success(
        "initial plan",
        &scenario.save_plan_with_refresh("initial.json"),
    );
    assert_success("initial apply", &scenario.apply_plan("initial.json"));

    scenario.write_readable_ignore_config("configured", Some("new"));
    assert_success(
        "saved update plan",
        &scenario.save_plan_with_refresh("tampered.json"),
    );
    let path = scenario.project.join("tampered.json");
    let mut saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read saved plan"))
            .expect("parse saved plan");
    assert!(
        tamper_authored_ignore_paths(&mut saved) > 0,
        "fixture must contain authored ignore paths: {saved:#}"
    );
    fs::write(
        &path,
        serde_json::to_vec_pretty(&saved).expect("serialize tampered saved plan"),
    )
    .expect("write tampered saved plan");

    let apply = scenario.apply_plan("tampered.json");
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "tampered plan applied:\n{rendered}"
    );
    assert!(
        rendered
            .contains("ignore_changes path `does_not_exist` does not exist in the resource schema"),
        "apply must reject the edited authored path through the shared resolver:\n{rendered}"
    );
}

#[test]
fn v32_saved_plan_is_rejected_after_keyed_release_format_bump() {
    let scenario = Scenario::new();
    scenario.write_readable_ignore_config("configured", None);
    assert_success("carina init", &scenario.init());
    assert_success(
        "current saved plan",
        &scenario.save_plan_with_refresh("old.json"),
    );

    let path = scenario.project.join("old.json");
    let mut saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read current saved plan"))
            .expect("parse current saved plan");
    saved["version"] = serde_json::json!(32);
    fs::write(
        &path,
        serde_json::to_vec_pretty(&saved).expect("serialize old saved plan"),
    )
    .expect("write old saved plan");

    let apply = scenario.apply_plan("old.json");
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "v32 saved plan applied:\n{rendered}"
    );
    assert!(
        rendered.contains("Unsupported plan file version: 32") && rendered.contains("plan --out"),
        "old plan must fail with replan guidance:\n{rendered}"
    );
}
