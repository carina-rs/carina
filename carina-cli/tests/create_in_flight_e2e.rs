use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::Effect;
use carina_state::StateFile;
use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    provider_block: String,
}

impl Scenario {
    fn new(resource: &str) -> Self {
        Self::new_with_provider(resource, "provider mock {}".to_string())
    }

    fn new_with_provider(resource: &str, provider_block: String) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
            provider_block,
        };
        scenario.write_config(resource);
        scenario
    }

    fn write_config(&self, resource: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                "backend local {{ path = \"carina.state.json\" }}\n\n{}\n\n{resource}\n",
                self.provider_block
            ),
        )
        .expect("write test configuration");
    }

    fn carina(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .args(args);
        command
    }

    fn init(&self) {
        let output = self
            .carina(&["init", "."])
            .output()
            .expect("run carina init");
        assert_success("carina init", &output);
    }

    fn apply_with_modes(&self, create: &str, resume: &str) -> Output {
        self.carina(&["apply", ".", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_CREATE_MODE", create)
            .env("CARINA_MOCK_RESUME_MODE", resume)
            .output()
            .expect("run carina apply")
    }

    fn save_plan(&self, refresh: bool, file: &str) -> (Output, PlanFile, String) {
        let mut args = vec!["plan"];
        if !refresh {
            args.push("--refresh=false");
        }
        args.extend(["--out", file, "."]);
        let output = self.carina(&args).output().expect("run carina plan");
        let raw = fs::read_to_string(self.project.join(file)).expect("read saved plan");
        let plan = serde_json::from_str(&raw).expect("decode saved plan");
        (output, plan, raw)
    }

    fn state_json(&self) -> Value {
        serde_json::from_str(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read local state"),
        )
        .expect("decode local state")
    }

    fn write_state_json(&self, state: &Value) {
        fs::write(
            self.project.join("carina.state.json"),
            serde_json::to_vec_pretty(state).expect("encode local state"),
        )
        .expect("write local state");
    }

    fn mock_state_json(&self) -> Value {
        serde_json::from_str(
            &fs::read_to_string(&self.mock_state).expect("read mock provider state"),
        )
        .expect("decode mock provider state")
    }

    fn write_mock_state_json(&self, state: &Value) {
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(state).expect("encode mock provider state"),
        )
        .expect("write mock provider state");
    }
}

fn wasm_mock_path() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .to_path_buf();
    ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_failed(label: &str, output: &Output) {
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn only_current(state: &Value) -> &Value {
    assert_eq!(state["version"], Value::from(StateFile::CURRENT_VERSION));
    &only_resource(state)["current"]
}

fn only_resource(state: &Value) -> &Value {
    let resources = state["resources"]
        .as_array()
        .expect("state resources must be an array");
    assert_eq!(resources.len(), 1, "fixture must write exactly one row");
    &resources[0]
}

fn assert_resume_only(plan: &PlanFile) {
    assert_resume_count(plan, 1);
}

fn assert_resume_count(plan: &PlanFile, expected: usize) {
    let resume_count = plan
        .plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::ResumeCreate { .. }))
        .count();
    let create_count = plan
        .plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Create(_resource)))
        .count();
    assert_eq!(
        resume_count, expected,
        "in-flight state must plan the expected resumes"
    );
    assert_eq!(create_count, 0, "in-flight state must never plan a create");
}

fn exercise_in_flight_cutover(resource: &str) {
    let scenario = Scenario::new(resource);
    scenario.init();

    let first = scenario.apply_with_modes("pending", "resumed");
    assert_failed("initial pending apply", &first);
    let first_state = scenario.state_json();
    let first_current = only_current(&first_state);
    assert_eq!(first_current["kind"], "create_in_flight");
    assert_eq!(first_current["envelope"]["token"], "builtin-mock-pending");
    let first_started_at = first_current["started_at"].clone();
    let first_envelope = first_current["envelope"].clone();

    for (refresh, file) in [(true, "live-plan.json"), (false, "cached-plan.json")] {
        let (output, plan, raw) = scenario.save_plan(refresh, file);
        assert_success("resume plan", &output);
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            rendered.contains("resume create") && rendered.contains("(in flight since "),
            "resume plan must identify the in-flight operation and its start time:\n{rendered}"
        );
        assert_resume_only(&plan);
        assert!(
            !raw.contains("builtin-mock-pending"),
            "saved plan leaked its state-owned provider token: {raw}"
        );
        assert!(
            raw.contains("create_in_flight"),
            "saved plan must retain the address-level in-flight marker: {raw}"
        );
    }

    let second = scenario.apply_with_modes("success", "pending");
    assert_failed("second over-budget apply", &second);
    let second_state = scenario.state_json();
    let second_current = only_current(&second_state);
    assert_eq!(second_current["kind"], "create_in_flight");
    assert_eq!(second_current["started_at"], first_started_at);
    assert_eq!(
        second_current["envelope"]["token"],
        "builtin-mock-repending"
    );
    let mut expected_envelope = first_envelope;
    expected_envelope["token"] = Value::String("builtin-mock-repending".to_string());
    assert_eq!(
        second_current["envelope"], expected_envelope,
        "a re-pend may replace only the token in the persisted envelope"
    );

    let final_apply = scenario.apply_with_modes("success", "resumed");
    assert_success("resumed apply", &final_apply);
    let final_state = scenario.state_json();
    let final_current = only_current(&final_state);
    assert_eq!(final_current["kind"], "applied");
    assert_eq!(final_current["identifier"], "mock-id");
}

#[test]
fn bound_create_in_flight_resumes_without_duplicate_create() {
    exercise_in_flight_cutover(
        r#"let pending = mock.test.resource {
  name = "bound-pending"
}"#,
    );
}

#[test]
fn anonymous_create_in_flight_resumes_without_duplicate_create() {
    exercise_in_flight_cutover(
        r#"mock.test.resource {
  name = "anonymous-pending"
}"#,
    );
}

#[test]
fn state_commands_report_create_in_flight_instead_of_empty_current_state() {
    let scenario = Scenario::new(
        r#"let pending = mock.test.resource {
  name = "state-command-pending"
}"#,
    );
    scenario.init();
    assert_failed(
        "initial pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );
    let state = scenario.state_json();
    let started_at = only_current(&state)["started_at"]
        .as_str()
        .expect("in-flight timestamp must be a string");
    let status = format!("create in flight since {started_at}");

    let list = scenario
        .carina(&["state", "list", "."])
        .output()
        .expect("run state list");
    assert_success("state list", &list);
    assert!(combined_output(&list).contains(&status));

    let lookup = scenario
        .carina(&["state", "lookup", "pending", "."])
        .output()
        .expect("run state lookup");
    assert_success("state lookup resource", &lookup);
    assert!(combined_output(&lookup).contains(&status));

    let attribute_lookup = scenario
        .carina(&["state", "lookup", "pending.name", "."])
        .output()
        .expect("run state lookup attribute");
    assert_failed("state lookup in-flight attribute", &attribute_lookup);
    let attribute_error = combined_output(&attribute_lookup);
    assert!(attribute_error.contains(&status));
    assert!(!attribute_error.contains("Attribute not found"));

    let show = scenario
        .carina(&["state", "show", "."])
        .output()
        .expect("run state show");
    assert_success("state show", &show);
    assert!(combined_output(&show).contains(&status));

    let refresh = scenario
        .carina(&["state", "refresh", "--lock=false", "."])
        .output()
        .expect("run state refresh");
    assert_success("state refresh", &refresh);
    assert!(combined_output(&refresh).contains(&status));
}

#[test]
fn wasm_bound_anonymous_moved_and_deposed_rows_resume_without_duplicate_create() {
    let Some(wasm_path) = wasm_mock_path() else {
        eprintln!(
            "skipping real-WASM CLI recovery test: build carina-provider-mock for wasm32-wasip2 first"
        );
        return;
    };
    let provider_block = format!(
        "provider mock {{\n  source = \"file://{}\"\n}}",
        wasm_path.display()
    );
    let scenario = Scenario::new_with_provider(
        r#"let old = mock.test.short_budget {
  create_mode = "resumed"
}

mock.test.short_budget {
  create_mode = "resumed"
}"#,
        provider_block,
    );
    scenario.init();

    let first = scenario.apply_with_modes("success", "resumed");
    assert_failed("WASM initial pending apply", &first);
    let mut first_state = scenario.state_json();
    let rows = first_state["resources"]
        .as_array_mut()
        .expect("WASM state resources must be an array");
    assert_eq!(rows.len(), 2, "bound and anonymous rows must both persist");
    assert!(
        rows.iter()
            .all(|row| row["current"]["kind"] == "create_in_flight")
    );
    let bound = rows
        .iter_mut()
        .find(|row| row["identity"] == "old")
        .expect("bound in-flight row must retain its authored identity");
    bound["deposed"] = serde_json::json!([{
        "key": "00000000-0000-4000-8000-000000000001",
        "identifier": "old-cloud-id",
        "attributes": {},
        "dependency_bindings": [],
    }]);
    scenario.write_state_json(&first_state);

    scenario.write_config(
        r#"moved {
  from = mock.test.short_budget 'old'
  to   = mock.test.short_budget 'new'
}

let new = mock.test.short_budget {
  create_mode = "resumed"
}

mock.test.short_budget {
  create_mode = "resumed"
}"#,
    );

    for (refresh, file) in [
        (true, "wasm-live-plan.json"),
        (false, "wasm-cached-plan.json"),
    ] {
        let (output, plan, raw) = scenario.save_plan(refresh, file);
        assert_success("WASM resume plan", &output);
        assert_resume_count(&plan, 2);
        assert!(
            plan.plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Move {
                    from: _from,
                    to: _to,
                    ..
                }
            )),
            "WASM plan must retain the in-flight move"
        );
        assert!(
            plan.plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Delete {
                    generation: carina_core::effect::EffectGeneration::Deposed(_key),
                    ..
                }
            )),
            "WASM plan must retain the deposed delete behind resume"
        );
        assert!(
            !raw.contains("remaining"),
            "saved plan leaked the WASM provider continuation token: {raw}"
        );
    }

    let resumed = scenario.apply_with_modes("success", "resumed");
    assert_success("WASM resumed apply", &resumed);
    let final_state = scenario.state_json();
    let rows = final_state["resources"]
        .as_array()
        .expect("final WASM state resources must be an array");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["current"]["kind"] == "applied"));
    assert!(rows.iter().any(|row| row["identity"] == "new"));
    assert!(rows.iter().all(|row| row["identity"] != "old"));
    let moved = rows
        .iter()
        .find(|row| row["identity"] == "new")
        .expect("moved row must be written at its destination");
    assert!(moved["deposed"].as_array().is_none_or(Vec::is_empty));
}

#[test]
fn moved_create_in_flight_rebinds_and_survives_another_pending_result() {
    let scenario = Scenario::new(
        r#"let old = mock.test.resource {
  name = "moved-pending"
}"#,
    );
    scenario.init();
    assert_failed(
        "initial pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );
    let first_state = scenario.state_json();
    let first_started_at = only_current(&first_state)["started_at"].clone();

    scenario.write_config(
        r#"moved {
  from = mock.test.resource 'old'
  to   = mock.test.resource 'new'
}

let new = mock.test.resource {
  name = "moved-pending"
}"#,
    );
    let (plan_output, plan, _raw) = scenario.save_plan(false, "moved-plan.json");
    assert_success("moved resume plan", &plan_output);
    assert_resume_only(&plan);
    assert!(
        plan.plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Move {
                from: _from,
                to: _to,
                ..
            }
        )),
        "moved in-flight plan must retain its explicit state move"
    );

    assert_failed(
        "moved re-pending apply",
        &scenario.apply_with_modes("success", "pending"),
    );
    let moved_state = scenario.state_json();
    let moved_row = only_resource(&moved_state);
    assert_eq!(moved_row["identity"], "new");
    assert_eq!(moved_row["current"]["kind"], "create_in_flight");
    assert_eq!(moved_row["current"]["started_at"], first_started_at);

    assert_success(
        "moved resumed apply",
        &scenario.apply_with_modes("success", "resumed"),
    );
    let final_state = scenario.state_json();
    let final_row = only_resource(&final_state);
    assert_eq!(final_row["identity"], "new");
    assert_eq!(final_row["current"]["kind"], "applied");
}

#[test]
fn moved_replacement_keeps_deposed_generation_until_resume_completes() {
    let scenario = Scenario::new(
        r#"let old = mock.test.resource {
  name = "before-moved-replacement"
  directives { create_before_destroy = true }
}"#,
    );
    scenario.init();
    assert_success(
        "initial successful apply",
        &scenario.apply_with_modes("success", "resumed"),
    );

    scenario.write_config(
        r#"let old = mock.test.resource {
  name = "after-moved-replacement"
  directives { create_before_destroy = true }
}"#,
    );
    assert_failed(
        "replacement pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );
    let pending_state = scenario.state_json();
    let pending_row = only_resource(&pending_state);
    assert_eq!(pending_row["current"]["kind"], "create_in_flight");
    assert_eq!(
        pending_row["deposed"]
            .as_array()
            .expect("replacement must retain its old generation")
            .len(),
        1
    );

    scenario.write_config(
        r#"moved {
  from = mock.test.resource 'old'
  to   = mock.test.resource 'new'
}

let new = mock.test.resource {
  name = "after-moved-replacement"
  directives { create_before_destroy = true }
}"#,
    );
    let delete_log = scenario.project.join("moved-replacement-delete.log");
    let moved_apply = scenario
        .carina(&["apply", ".", "--auto-approve", "--lock=false"])
        .env("CARINA_MOCK_CREATE_MODE", "success")
        .env("CARINA_MOCK_RESUME_MODE", "pending")
        .env("CARINA_MOCK_DELETE_LOG", &delete_log)
        .output()
        .expect("run moved replacement apply");
    assert_failed("moved replacement re-pending apply", &moved_apply);
    assert!(
        fs::read_to_string(&delete_log)
            .unwrap_or_default()
            .is_empty(),
        "the deposed delete must not reach the provider before the post-move resume completes"
    );

    let moved_state = scenario.state_json();
    assert_eq!(
        moved_state["resources"]
            .as_array()
            .expect("moved state resources must be an array")
            .len(),
        1,
        "the moved row must be re-keyed instead of duplicated: {moved_state:#}"
    );
    let moved_row = only_resource(&moved_state);
    assert_eq!(moved_row["identity"], "new");
    assert_eq!(moved_row["current"]["kind"], "create_in_flight");
    assert_eq!(
        moved_row["deposed"]
            .as_array()
            .expect("failed moved resume must retain the old generation")
            .len(),
        1,
        "the deposed delete must wait for the post-move ResumeCreate"
    );
}

fn moved_pending_replacement_fixture() -> Scenario {
    let scenario = Scenario::new(
        r#"let old = mock.test.resource {
  name = "before-moved-history"
  directives { create_before_destroy = true }
}"#,
    );
    scenario.init();
    assert_success(
        "initial successful apply",
        &scenario.apply_with_modes("success", "resumed"),
    );

    scenario.write_config(
        r#"let old = mock.test.resource {
  name = "after-moved-history"
  directives { create_before_destroy = true }
}"#,
    );
    assert_failed(
        "replacement pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );

    let mut state = scenario.state_json();
    let deposed = state["resources"][0]["deposed"]
        .as_array_mut()
        .expect("replacement must retain its old generation");
    assert_eq!(deposed.len(), 1);
    // The builtin mock uses one provider identifier for every create. Give
    // the old generation its own cloud identity so a failed Delete cannot be
    // mistaken for the newly completed current instance during writeback.
    deposed[0]["identifier"] = Value::String("old-cloud-id".to_string());
    scenario.write_state_json(&state);

    scenario.write_config(
        r#"moved {
  from = mock.test.resource 'old'
  to   = mock.test.resource 'new'
}

let new = mock.test.resource {
  name = "after-moved-history"
  directives { create_before_destroy = true }
}"#,
    );
    scenario
}

fn assert_moved_deposed_generation_is_retained(scenario: &Scenario) {
    let state = scenario.state_json();
    let rows = state["resources"]
        .as_array()
        .expect("moved state resources must be an array");
    assert_eq!(
        rows.len(),
        1,
        "the move must leave one destination row: {state:#}"
    );
    let row = only_resource(&state);
    assert_eq!(row["identity"], "new");
    let deposed = row["deposed"]
        .as_array()
        .expect("the destination row must retain the old generation");
    assert_eq!(deposed.len(), 1, "state lost moved history: {state:#}");
    assert_eq!(deposed[0]["identifier"], "old-cloud-id");
}

#[test]
fn moved_create_ended_keeps_deposed_generation_when_delete_is_blocked() {
    let scenario = moved_pending_replacement_fixture();
    let output = scenario.apply_with_modes("success", "discarded");
    assert_failed("moved ended resume", &output);

    let rendered = combined_output(&output);
    assert!(
        !rendered.contains("CARINA_MOCK_DELETE_FAIL_FOR"),
        "the deposed Delete must remain blocked when ResumeCreate ends"
    );
    assert_moved_deposed_generation_is_retained(&scenario);
    assert_eq!(only_current(&scenario.state_json())["kind"], "none");
}

#[test]
fn moved_observed_resume_results_keep_deposed_generation_until_delete_succeeds() {
    for (resume_mode, label, delete_runs) in [
        ("created_read_failed", "created", false),
        ("resumed", "resumed", true),
    ] {
        let scenario = moved_pending_replacement_fixture();
        let output = scenario
            .carina(&["apply", ".", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_CREATE_MODE", "success")
            .env("CARINA_MOCK_RESUME_MODE", resume_mode)
            .env("CARINA_MOCK_DELETE_FAIL_FOR", "*")
            .env("CARINA_MOCK_DELETE_FAIL_IDENTIFIER", "old-cloud-id")
            .output()
            .expect("run moved observed-result apply");
        assert_failed(&format!("moved {label} resume"), &output);

        let rendered = combined_output(&output);
        assert_eq!(
            rendered.contains("CARINA_MOCK_DELETE_FAIL_FOR"),
            delete_runs,
            "unexpected deposed Delete scheduling for {label}:\n{rendered}"
        );
        assert_moved_deposed_generation_is_retained(&scenario);
        assert_eq!(only_current(&scenario.state_json())["kind"], "applied");
    }
}

#[test]
fn moved_present_row_keeps_deposed_generation_when_delete_fails() {
    let scenario = Scenario::new(
        r#"let old = mock.test.resource {
  name = "moved-present"
}"#,
    );
    scenario.init();
    assert_success(
        "initial successful apply",
        &scenario.apply_with_modes("success", "resumed"),
    );

    let mut state = scenario.state_json();
    state["resources"][0]["deposed"] = serde_json::json!([{
        "key": "00000000-0000-4000-8000-000000000002",
        "identifier": "old-cloud-id",
        "attributes": {},
        "dependency_bindings": [],
    }]);
    scenario.write_state_json(&state);
    scenario.write_config(
        r#"moved {
  from = mock.test.resource 'old'
  to   = mock.test.resource 'new'
}

let new = mock.test.resource {
  name = "moved-present"
}"#,
    );

    let output = scenario
        .carina(&["apply", ".", "--auto-approve", "--lock=false"])
        .env("CARINA_MOCK_CREATE_MODE", "success")
        .env("CARINA_MOCK_RESUME_MODE", "resumed")
        .env("CARINA_MOCK_DELETE_FAIL_FOR", "*")
        .env("CARINA_MOCK_DELETE_FAIL_IDENTIFIER", "old-cloud-id")
        .output()
        .expect("run moved present-row apply");
    assert_failed("moved present-row apply", &output);
    assert!(combined_output(&output).contains("CARINA_MOCK_DELETE_FAIL_FOR"));
    assert_moved_deposed_generation_is_retained(&scenario);
    assert_eq!(only_current(&scenario.state_json())["kind"], "applied");
}

#[test]
fn moved_create_in_flight_rejects_a_changed_provider_instance() {
    let scenario = Scenario::new_with_provider(
        r#"let old = mock.test.resource {
  name = "moved-route"
  directives { provider = east }
}"#,
        r#"provider mock {}

let east = provider mock {}
let west = provider mock {}"#
            .to_string(),
    );
    scenario.init();
    assert_failed(
        "initial pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );
    assert_eq!(
        only_resource(&scenario.state_json())["directives"]["provider_instance"],
        "east"
    );

    scenario.write_config(
        r#"moved {
  from = mock.test.resource 'old'
  to   = mock.test.resource 'new'
}

let new = mock.test.resource {
  name = "moved-route"
  directives { provider = west }
}"#,
    );
    let output = scenario
        .carina(&["plan", "--refresh=false", "."])
        .output()
        .expect("run moved provider-routing plan");
    assert_failed("moved provider-routing plan", &output);
    let rendered = combined_output(&output);
    assert!(
        rendered.contains("saved create continuation belongs to provider route")
            && rendered.contains("east"),
        "plan must report ResumeCreateRoutingMismatch for the issuing route:\n{rendered}"
    );
    assert!(
        !rendered.contains("resume create"),
        "a routing mismatch must not plan ResumeCreate:\n{rendered}"
    );
}

#[test]
fn replacement_create_in_flight_keeps_deposed_generation_until_resume() {
    let scenario = Scenario::new(
        r#"let pending = mock.test.resource {
  name = "before-replacement"
  directives { create_before_destroy = true }
}"#,
    );
    scenario.init();
    assert_success(
        "initial successful apply",
        &scenario.apply_with_modes("success", "resumed"),
    );

    scenario.write_config(
        r#"let pending = mock.test.resource {
  name = "after-replacement"
  directives { create_before_destroy = true }
}"#,
    );
    assert_failed(
        "replacement pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );
    let pending_state = scenario.state_json();
    let pending_row = only_resource(&pending_state);
    assert_eq!(pending_row["current"]["kind"], "create_in_flight");
    assert_eq!(
        pending_row["deposed"]
            .as_array()
            .expect("replacement row must carry deposed generations")
            .len(),
        1
    );

    let (plan_output, plan, _raw) = scenario.save_plan(false, "replacement-plan.json");
    assert_success("replacement resume plan", &plan_output);
    assert_resume_only(&plan);
    assert!(
        plan.plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Delete {
                id: _id,
                identifier: _identifier,
                generation: carina_core::effect::EffectGeneration::Deposed(_key),
                directives: _directives,
                binding: _binding,
                dependencies: _dependencies,
                explicit_dependencies: _explicit_dependencies,
                blocked_by_updates: _blocked_by_updates,
            }
        )),
        "the old generation must remain scheduled for deletion"
    );

    assert_success(
        "replacement resumed apply",
        &scenario.apply_with_modes("success", "resumed"),
    );
    let final_state = scenario.state_json();
    let final_row = only_resource(&final_state);
    assert_eq!(final_row["current"]["kind"], "applied");
    assert!(
        final_row["deposed"].as_array().is_none_or(Vec::is_empty),
        "the deposed generation may leave state only after its delete succeeds"
    );
}

#[test]
fn discarded_resume_clears_only_current_and_keeps_deposed_generation() {
    let scenario = Scenario::new(
        r#"let pending = mock.test.resource {
  name = "before-discard"
  directives { create_before_destroy = true }
}"#,
    );
    scenario.init();
    assert_success(
        "initial successful apply",
        &scenario.apply_with_modes("success", "resumed"),
    );
    scenario.write_config(
        r#"let pending = mock.test.resource {
  name = "after-discard"
  directives { create_before_destroy = true }
}"#,
    );
    assert_failed(
        "replacement pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );

    let discarded = scenario.apply_with_modes("success", "discarded");
    assert_failed("discarded resume apply", &discarded);
    let state = scenario.state_json();
    let row = only_resource(&state);
    assert_eq!(row["current"]["kind"], "none");
    assert_eq!(
        row["deposed"]
            .as_array()
            .expect("discarded replacement must retain its old generation")
            .len(),
        1,
        "only the provider-ended current slot may be cleared"
    );

    let plan_output = scenario
        .carina(&[
            "plan",
            "--refresh=false",
            "--out",
            "discarded-next-plan.json",
            ".",
        ])
        .output()
        .expect("run plan after discarded resume");
    assert_success("plan after discarded resume", &plan_output);
    let plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("discarded-next-plan.json"))
            .expect("read plan after discarded resume"),
    )
    .expect("decode plan after discarded resume");
    assert_eq!(
        plan.plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::Create(_resource)))
            .count(),
        1,
        "the absent current instance must be recreated"
    );
    assert_eq!(
        plan.plan
            .effects()
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Delete {
                    generation: carina_core::effect::EffectGeneration::Deposed(_key),
                    ..
                }
            ))
            .count(),
        1,
        "the retained deposed generation must still be deleted"
    );
}

#[test]
fn issued_untracked_resume_stays_in_flight() {
    let scenario = Scenario::new(
        r#"let pending = mock.test.resource {
  name = "issued-untracked"
}"#,
    );
    scenario.init();
    assert_failed(
        "initial pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );

    // Model a create that learned its identifier while it was pending. The
    // persisted identifier selects the issued resume witness on the next run.
    let mut state = scenario.state_json();
    state["resources"][0]["current"]["envelope"]["identifier"] =
        Value::String("mock-id".to_string());
    scenario.write_state_json(&state);

    let resumed = scenario.apply_with_modes("success", "untracked");
    assert_failed("issued untracked resume", &resumed);
    let state = scenario.state_json();
    let current = only_current(&state);
    assert_eq!(current["kind"], "create_in_flight");
    assert_eq!(current["envelope"]["identifier"], "mock-id");

    let (output, plan, _raw) = scenario.save_plan(false, "issued-untracked-next.json");
    assert_success("plan after issued untracked resume", &output);
    assert_resume_only(&plan);
}

#[test]
fn created_read_failed_resume_marks_unknowns_without_replacing_or_regenerating_prefix() {
    let scenario = Scenario::new(
        r#"let pending = mock.test.resource {
  name_prefix = "resume-read-failed-"
}"#,
    );
    scenario.init();
    assert_failed(
        "initial pending apply",
        &scenario.apply_with_modes("pending", "resumed"),
    );

    let resumed = scenario.apply_with_modes("success", "created_read_failed");
    assert_failed("created with failed read resume", &resumed);
    let state = scenario.state_json();
    let current = only_current(&state);
    assert_eq!(current["kind"], "applied");
    assert_eq!(current["identifier"], "mock-id");
    assert!(
        current["partial_read"]["missing_attributes"]
            .as_array()
            .is_some_and(|attributes| attributes.iter().any(|attr| attr == "name")),
        "the failed observation must persist a marker for the missing generated name: {current:#}"
    );

    let cached_output = scenario
        .carina(&[
            "plan",
            "--refresh=false",
            "--out",
            "read-failed-cached-plan.json",
            ".",
        ])
        .output()
        .expect("run cached plan after failed post-create read");
    assert_success("cached plan after failed post-create read", &cached_output);
    let cached_plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("read-failed-cached-plan.json"))
            .expect("read cached plan"),
    )
    .expect("decode cached plan");
    assert!(
        !cached_plan.plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Create(_)
                | Effect::Delete {
                    generation: carina_core::effect::EffectGeneration::Current,
                    ..
                }
        )),
        "a cached plan must not replace a resource whose post-create values are unknown: {:#?}",
        cached_plan.plan.effects()
    );
    assert!(matches!(
        cached_plan.sorted_resources[0].attributes.get("name"),
        Some(carina_core::resource::Value::Deferred(
            carina_core::resource::DeferredValue::Unknown(
                carina_core::resource::UnknownReason::PendingReRead(_)
            )
        ))
    ));

    // Make the next provider observation impossible to confuse with a newly
    // generated fallback. The exact observed value must replace the stale
    // pending re-read unknown in the refreshed desired resource.
    let provider_returned_name = "provider-returned-exact-name";
    let mut provider_state = scenario.mock_state_json();
    provider_state["states"]["test.resource.pending"]["name"] =
        Value::String(provider_returned_name.to_string());
    scenario.write_mock_state_json(&provider_state);

    let read_outcome = scenario.project.join("read-outcome.txt");
    let live_output = scenario
        .carina(&["plan", "--out", "read-failed-live-plan.json", "."])
        .env("CARINA_MOCK_READ_OUTCOME_PATH", &read_outcome)
        .env("CARINA_MOCK_READ_DELAY_MS_FOR", "test.resource.pending")
        .env("CARINA_MOCK_READ_DELAY_MS", "1")
        .output()
        .expect("run refresh-enabled plan after failed post-create read");
    assert_success(
        "refresh-enabled plan after failed post-create read",
        &live_output,
    );
    assert_eq!(
        fs::read_to_string(read_outcome).expect("refresh must report its read outcome"),
        "completed\n",
        "the next refresh-enabled plan must retry the provider read"
    );
    let live_plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("read-failed-live-plan.json"))
            .expect("read refresh-enabled plan"),
    )
    .expect("decode refresh-enabled plan");
    assert!(
        !live_plan.plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Create(_)
                | Effect::Delete {
                    generation: carina_core::effect::EffectGeneration::Current,
                    ..
                }
        )),
        "the refreshed resource must not be replaced: {:#?}",
        live_plan.plan.effects()
    );
    let refreshed_name = live_plan.sorted_resources[0]
        .attributes
        .get("name")
        .expect("the refreshed desired resource keeps its generated name");
    assert!(
        matches!(
            refreshed_name,
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(name)
            ) if name.expose_plaintext() == provider_returned_name
        ),
        "a successful refresh must use the exact provider-returned name {provider_returned_name:?}, got {refreshed_name:?}"
    );
}

fn applied_prefixed_name(scenario: &Scenario) -> String {
    let state = scenario.state_json();
    only_current(&state)["attributes"]["name"]
        .as_str()
        .expect("applied prefix fixture stores its generated name")
        .to_string()
}

fn assert_plan_keeps_prefixed_name(plan: &PlanFile, expected: &str) {
    assert!(
        matches!(
            plan.sorted_resources[0].attributes.get("name"),
            Some(carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(name)
            )) if name.expose_plaintext() == expected
        ),
        "refreshed desired name must stay equal to the persisted generated name {expected:?}, got {:?}",
        plan.sorted_resources[0].attributes.get("name")
    );
}

#[test]
fn absent_refresh_recreates_with_the_stored_prefixed_name() {
    let scenario = Scenario::new(
        r#"let prefixed = mock.test.resource {
  name_prefix = "absent-refresh-"
}"#,
    );
    scenario.init();
    let apply = scenario.apply_with_modes("success", "resumed");
    assert_success("initial prefixed apply", &apply);
    let stored_name = applied_prefixed_name(&scenario);

    let mut provider_state = scenario.mock_state_json();
    provider_state["states"] = serde_json::json!({});
    scenario.write_mock_state_json(&provider_state);

    let (output, plan, _raw) = scenario.save_plan(true, "absent-prefix-plan.json");
    assert_success("plan after out-of-band deletion", &output);
    assert_plan_keeps_prefixed_name(&plan, &stored_name);
    assert!(
        plan.plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Create(_))),
        "the absent resource must be recreated: {:#?}",
        plan.plan.effects()
    );
}

#[test]
fn refresh_omitting_prefixed_name_keeps_stored_desired_name() {
    let scenario = Scenario::new(
        r#"let prefixed = mock.test.resource {
  name_prefix = "omitted-refresh-"
}"#,
    );
    scenario.init();
    let apply = scenario.apply_with_modes("success", "resumed");
    assert_success("initial prefixed apply", &apply);
    let stored_name = applied_prefixed_name(&scenario);

    let mut provider_state = scenario.mock_state_json();
    provider_state["states"]["test.resource.prefixed"]
        .as_object_mut()
        .expect("mock resource attributes are an object")
        .remove("name");
    scenario.write_mock_state_json(&provider_state);

    let (output, plan, _raw) = scenario.save_plan(true, "omitted-prefix-plan.json");
    assert_success("plan after provider omitted prefixed name", &output);
    assert_plan_keeps_prefixed_name(&plan, &stored_name);
}
