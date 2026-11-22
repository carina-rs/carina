//! Previous hashes release only for accepted public writes or equal public values.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use carina_core::{
    resource::{ConcreteValue, RedactedSecretHash, SnapshotValue, Value as CoreValue},
    value::PERSISTED_SECRET_KEY,
};
use serde_json::Value;
use tempfile::TempDir;

const RETAINED_CALL: &str = "g(src, lower(src.identifier))";

struct Scenario {
    project: TempDir,
    source: PathBuf,
    marker: String,
    module: bool,
}

impl Scenario {
    fn new(module: bool, sink: &str) -> Self {
        let project = tempfile::tempdir().expect("project directory");
        // Generate the marker at runtime and mask it before rendering failures.
        let marker = format!(
            "https://refresh-{}.invalid",
            project
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .trim_start_matches('.')
        )
        .to_ascii_lowercase();
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/state_refresh_retained_secret");
        fs::write(
            project.path().join("providers.crn"),
            "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
        )
        .unwrap();
        let directory = if module {
            let directory = project.path().join("retained");
            fs::create_dir(&directory).unwrap();
            fs::copy(
                fixture.join("arguments.crn"),
                directory.join("arguments.crn"),
            )
            .unwrap();
            fs::write(
                project.path().join("main.crn"),
                "let retained = use { source = './retained' }\nlet instance = retained { label = 'instance' }\n",
            )
            .unwrap();
            directory
        } else {
            project.path().to_path_buf()
        };
        let source = directory.join("resources.crn");
        fs::write(
            &source,
            fs::read_to_string(fixture.join("main.crn"))
                .unwrap()
                .replace(RETAINED_CALL, sink)
                .replace("__MARKER__", &marker),
        )
        .unwrap();
        Self {
            project,
            source,
            marker,
            module,
        }
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.project.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.project.path().join("provider-state.json"),
            )
            .args(args)
            .output()
            .expect("run carina")
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let masked =
            format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&self.marker, "<MASKED>");
        assert!(output.status.success(), "{args:?} failed:\n{masked}");
        assert!(
            !stdout.contains(&self.marker) && !stderr.contains(&self.marker),
            "{args:?} exposed the secret marker:\n{masked}"
        );
        masked
    }

    fn state(&self) -> Value {
        let bytes = fs::read(self.project.path().join("carina.state.json")).unwrap();
        serde_json::from_slice(&bytes).expect("valid state JSON")
    }

    fn set_provider_computed(&self, name: &str, material: &str) {
        let path = self.project.path().join("provider-state.json");
        let mut provider: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let row = provider["states"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .find(|row| row["name"] == name)
            .expect("provider row");
        row["computed"] = Value::from(material);
        fs::write(&path, serde_json::to_vec(&provider).unwrap()).unwrap();
    }

    fn computed(&self, state: &Value, binding: &str) -> Value {
        let binding = if self.module {
            format!("instance.{binding}")
        } else {
            binding.to_owned()
        };
        let row = state["resources"]
            .as_array()
            .expect("state resources")
            .iter()
            .find(|row| row["binding"] == binding)
            .unwrap_or_else(|| panic!("missing row for {binding}"));
        row["current"]["attributes"]["computed"].clone()
    }

    fn assert_hash(&self, state: &Value, binding: &str, material: &str) -> String {
        let computed = self.computed(state, binding);
        let digest = computed[PERSISTED_SECRET_KEY]
            .as_str()
            .unwrap_or_else(|| panic!("{binding}.computed must remain a hash"));
        let hash = RedactedSecretHash::from_encoded_digest(digest).expect("valid hash");
        let snapshot = SnapshotValue::redacted(hash);
        let value: CoreValue = CoreValue::Concrete(ConcreteValue::String(material.into()));
        assert!(
            snapshot.verify_secret(&value).expect("verify hash"),
            "{binding}.computed must hash the provider material"
        );
        assert!(
            !serde_json::to_string(state).unwrap().contains(&self.marker),
            "state must not contain the secret marker"
        );
        digest.to_owned()
    }

    fn assert_plan_artifacts_hide_old_material(&self, changes: bool) {
        self.assert_plan_artifacts(Some(changes));
    }

    fn assert_plan_artifacts_do_not_expose_old_material(&self) {
        self.assert_plan_artifacts(None);
    }

    fn assert_plan_artifacts(&self, expected_changes: Option<bool>) {
        for refresh in [true, false] {
            let refresh_flag = format!("--refresh={refresh}");
            let output = self.command(&["plan", &refresh_flag, "--json", "."]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let safe =
                format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&self.marker, "<MASKED>");
            assert!(output.status.success(), "JSON plan failed:\n{safe}");
            assert!(
                !stdout.contains(&self.marker) && !stderr.contains(&self.marker),
                "JSON plan exposed old material:\n{safe}"
            );
            let json: Value = serde_json::from_str(&stdout).expect("valid JSON plan");
            let count = json["plan"]["effects"]
                .as_array()
                .expect("JSON effects")
                .len();
            if let Some(changes) = expected_changes {
                assert_eq!(
                    count != 0,
                    changes,
                    "JSON plan must preserve the measured effect count"
                );
            }
            let file = if refresh {
                "fresh-public.plan.json"
            } else {
                "public.plan.json"
            };
            self.run(&["plan", &refresh_flag, "--out", file, "."]);
            let saved = fs::read_to_string(self.project.path().join(file)).unwrap();
            assert!(
                !saved.contains(&self.marker),
                "saved plan exposed old material"
            );
            let saved: Value = serde_json::from_str(&saved).expect("valid saved plan");
            assert_eq!(
                saved["plan"]["effects"]
                    .as_array()
                    .expect("saved effects")
                    .len(),
                count,
                "saving must not introduce or remove an effect"
            );
        }
    }

    fn apply_and_plan(&self, sink_material: &str) -> String {
        self.run(&["init", "."]);
        self.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
        let applied = self.state();
        self.assert_hash(&applied, "src", &self.marker);
        let digest = self.assert_hash(&applied, "sink", sink_material);
        let plan = self.run(&["plan", "."]);
        assert!(plan.contains("No changes."), "{plan}");
        assert!(self.state() == applied, "plan must not change state");
        digest
    }

    fn refresh_hash(&self, sink_material: &str) -> (String, String) {
        // Check persisted state before the output assertion, so a failing test
        // reports both the state leak and the masked refresh change line.
        let output = self.command(&["state", "refresh", "."]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let state = self.state();
        let is_hash = self.computed(&state, "sink")[PERSISTED_SECRET_KEY].is_string();
        let safe_output =
            format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&self.marker, "<MASKED>");
        assert!(
            output.status.success()
                && is_hash
                && !stdout.contains(&self.marker)
                && !stderr.contains(&self.marker),
            "refresh must keep sink.computed hashed and never print the marker (state hash: {is_hash}):\n{safe_output}"
        );
        (self.assert_hash(&state, "sink", sink_material), safe_output)
    }

    fn assert_refresh_protected(&self, sink_material: &str) -> String {
        let before = self.apply_and_plan(sink_material);
        let (after, _) = self.refresh_hash(sink_material);
        self.assert_hash(&self.state(), "src", &self.marker);
        assert_eq!(
            after, before,
            "unchanged refresh must reuse the previous digest"
        );
        self.assert_plan_artifacts_do_not_expose_old_material();
        before
    }

    fn assert_drift_protected(&self, before: &str) {
        // Exercise a real change line and verify that refresh hashes fresh
        // provider material instead of copying the previous hash.
        let drifted = format!("drift-{}", self.marker);
        self.set_provider_computed("sink", &drifted);
        let (refreshed, output) = self.refresh_hash(&drifted);
        assert!(
            output.contains("computed: (secret) → (secret)"),
            "provider drift must render a protected change line:\n{output}"
        );
        assert_ne!(refreshed, before, "provider drift must change the digest");
        self.assert_plan_artifacts_do_not_expose_old_material();
    }
}

#[test]
fn refresh_retained_secret_call_at_root_keeps_previous_hash() {
    let scenario = Scenario::new(false, RETAINED_CALL);
    let before = scenario.assert_refresh_protected(&format!("prefix-{}", scenario.marker));
    scenario.assert_drift_protected(&before);
}

#[test]
fn refresh_retained_secret_call_in_module_keeps_previous_hash() {
    let scenario = Scenario::new(true, RETAINED_CALL);
    let before = scenario.assert_refresh_protected(&format!("prefix-{}", scenario.marker));
    scenario.assert_drift_protected(&before);
}

#[test]
fn refresh_retained_call_with_visible_secret_argument_keeps_previous_hash() {
    use carina_core::resource::{DeferredValue, Value as ResourceValue};

    let scenario = Scenario::new(
        false,
        "visible(lookup({ 'src-id' = src.computed }, src.identifier, secret('__MARKER__')))",
    );
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(&scenario.source, format!("fn visible(x) {{ x }}\n{source}")).unwrap();
    let parsed = carina_core::config_loader::parse_directory(
        &carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap(),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap_or_else(|_| panic!("the unannotated visible fixture must parse"))
    .into_parsed();
    let sink = parsed
        .resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str() == "sink")
        })
        .expect("the fixture must retain its sink resource");
    let computed = &sink.attributes["computed"];
    assert!(
        matches!(computed, ResourceValue::Deferred(DeferredValue::FunctionCall { name, .. }) if name == "visible")
            || matches!(computed, ResourceValue::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) if function.name == "visible"),
        "visible(...) must remain a deferred call until its resource inputs resolve at apply"
    );
    scenario.assert_refresh_protected(&scenario.marker);
}

#[test]
fn refresh_secret_reference_keeps_previous_hash() {
    let scenario = Scenario::new(false, "src.tags.nested");
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(
        &scenario.source,
        source.replace(
            "name = 'src'",
            &format!(
                "name = 'src'\n  tags = {{ nested = secret('{}') }}",
                scenario.marker
            ),
        ),
    )
    .unwrap();
    let before = scenario.assert_refresh_protected(&scenario.marker);
    scenario.assert_drift_protected(&before);
}

#[test]
fn refresh_unequal_public_reference_keeps_hash_until_accepted_apply_releases_it() {
    let scenario = Scenario::new(false, RETAINED_CALL);
    let old_material = format!("prefix-{}", scenario.marker);
    let before = scenario.apply_and_plan(&old_material);
    let state = scenario.state();
    let identifier = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "src")
        .expect("source row")["current"]["attributes"]["identifier"]
        .as_str()
        .expect("public computed identifier");
    let source = fs::read_to_string(&scenario.source).unwrap();
    // The reference resolves to public material, but the provider still holds
    // the old secret. Equality must not release that different provider value.
    fs::write(
        &scenario.source,
        source.replace(RETAINED_CALL, "src.identifier"),
    )
    .unwrap();
    let (after, _) = scenario.refresh_hash(&old_material);
    assert_eq!(after, before, "unchanged material keeps its previous hash");
    for args in [vec!["plan", "."], vec!["plan", "--refresh=false", "."]] {
        let plan = scenario.run(&args);
        assert!(plan.contains("~ mock.test.resource sink"), "{plan}");
        assert!(plan.contains("(secret)"), "{plan}");
    }
    scenario.assert_plan_artifacts_hide_old_material(true);
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    assert!(scenario.computed(&scenario.state(), "sink") == identifier);
    assert!(
        !serde_json::to_string(&scenario.state())
            .unwrap()
            .contains(&scenario.marker),
        "accepted public write must not expose old material"
    );
    for args in [vec!["plan", "."], vec!["plan", "--refresh=false", "."]] {
        let plan = scenario.run(&args);
        assert!(plan.contains("No changes."), "{plan}");
    }
    scenario.assert_plan_artifacts_hide_old_material(false);
}

#[test]
fn refresh_unequal_public_literal_keeps_hash_until_accepted_apply_releases_it() {
    let scenario = Scenario::new(false, RETAINED_CALL);
    scenario.apply_and_plan(&format!("prefix-{}", scenario.marker));
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(&scenario.source, source.replace(RETAINED_CALL, "'public'")).unwrap();
    scenario.set_provider_computed("sink", "provider-public");
    scenario.run(&["state", "refresh", "."]);
    scenario.assert_hash(&scenario.state(), "sink", "provider-public");
    scenario.run(&["apply", "--auto-approve", "--parallelism=1", "."]);
    assert!(
        scenario.computed(&scenario.state(), "sink") == "public",
        "the accepted public update releases the previous hash"
    );
    let plan = scenario.run(&["plan", "."]);
    assert!(plan.contains("No changes."), "{plan}");
    scenario.assert_plan_artifacts_do_not_expose_old_material();
}

// Capture masked evidence before asserting, including state after failed writes.
struct Trace<'a> {
    case: &'a str,
    scenario: &'a Scenario,
    steps: Vec<Value>,
}

impl<'a> Trace<'a> {
    fn new(case: &'a str, scenario: &'a Scenario) -> Self {
        Self {
            case,
            scenario,
            steps: Vec::new(),
        }
    }

    fn command(&mut self, label: &str, args: &[&str], fail_update: Option<&str>) -> usize {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(self.scenario.project.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("CARINA_MOCK_UPDATE_FAIL_FOR")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.scenario.project.path().join("provider-state.json"),
            )
            .args(args);
        if let Some(target) = fail_update {
            command.env("CARINA_MOCK_UPDATE_FAIL_FOR", target);
        }
        let output = command.output().expect("run measurement command");
        self.capture(label, args, Some(output))
    }

    fn snapshot(&mut self, label: &str) {
        self.capture(label, &[], None);
    }

    fn capture(&mut self, label: &str, args: &[&str], output: Option<Output>) -> usize {
        let stdout = output
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let stderr = output
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
            .unwrap_or_default();
        let state_text =
            fs::read_to_string(self.scenario.project.path().join("carina.state.json")).ok();
        let state: Option<Value> = state_text
            .as_ref()
            .map(|text| serde_json::from_str(text).expect("parse measurement state"));
        let computed = state
            .as_ref()
            .map(|state| self.scenario.computed(state, "sink"));
        let marker = &self.scenario.marker;
        let summary = stdout.lines().find(|line| {
            line.starts_with("Plan: ")
                || line.contains("No changes.")
                || line.contains("No changes needed.")
                || line.contains("No changes to apply.")
        });
        let planned_updates = stdout
            .lines()
            .filter(|line| summary.is_some() && line.trim_start().starts_with("~ "))
            .map(|line| line.trim().replace(marker, "<MASKED>"))
            .collect::<Vec<_>>();
        let step = serde_json::json!({
            "step": label,
            "args": args,
            "exit_code": output.as_ref().and_then(|output| output.status.code()),
            "sink_hash": computed.as_ref().map(|value| value[PERSISTED_SECRET_KEY].is_string()),
            "sink_plaintext": computed.as_ref().and_then(Value::as_str).map(|text| text.replace(marker, "<MASKED>")),
            "state_contains_marker": state_text.as_ref().is_some_and(|text| text.contains(marker)),
            "stdout_contains_marker": stdout.contains(marker),
            "stderr_contains_marker": stderr.contains(marker),
            "planned_changes": summary.map(|line| !line.contains("No changes")),
            "plan_summary": summary.map(|line| line.replace(marker, "<MASKED>")),
            "update_planned": !planned_updates.is_empty(),
            "sink_update_planned": planned_updates.iter().any(|line| line.split_whitespace().last() == Some("sink")),
            "planned_updates": planned_updates,
            "stdout": stdout.replace(marker, "<MASKED>"),
            "stderr": stderr.replace(marker, "<MASKED>"),
        });
        self.steps.push(step);
        let evidence = serde_json::json!({
            "case": self.case,
            "binary": env!("CARGO_BIN_EXE_carina"),
            "steps": self.steps,
        });
        let masked = serde_json::to_string_pretty(&evidence).unwrap();
        assert!(
            !masked.contains(marker),
            "measurement evidence must mask the marker"
        );
        self.steps.len() - 1
    }

    fn assert_exit(&self, index: usize, success: bool) {
        let step = &self.steps[index];
        let matches = if success {
            step["exit_code"] == 0
        } else {
            step["exit_code"].as_i64().is_some_and(|code| code != 0)
                && step["stderr"]
                    .as_str()
                    .unwrap()
                    .contains("CARINA_MOCK_UPDATE_FAIL_FOR requested update failure")
        };
        assert!(
            matches,
            "{}: unexpected exit:\n{}",
            self.case,
            self.render()
        );
    }

    fn no_marker(&self) -> bool {
        self.steps.iter().all(|step| {
            step["state_contains_marker"] == false
                && step["stdout_contains_marker"] == false
                && step["stderr_contains_marker"] == false
        })
    }

    fn protected(&self) -> bool {
        self.no_marker()
            && self
                .steps
                .iter()
                .all(|step| step["sink_hash"].is_null() || step["sink_hash"] == true)
    }

    fn render(&self) -> String {
        serde_json::to_string_pretty(&self.steps).unwrap()
    }
}

fn retained_fixture(shape: &str) -> Scenario {
    let scenario = Scenario::new(
        shape == "module",
        if shape == "segmented" {
            "src.tags.nested"
        } else {
            RETAINED_CALL
        },
    );
    if shape == "segmented" {
        let source = fs::read_to_string(&scenario.source).unwrap();
        fs::write(
            &scenario.source,
            source.replace(
                "name = 'src'",
                &format!(
                    "name = 'src'\n  tags = {{ nested = secret('{}') }}",
                    scenario.marker
                ),
            ),
        )
        .unwrap();
    }
    scenario
}

fn assert_carried_apply_protected(shape: &str, fail: bool) {
    let scenario = retained_fixture(shape);
    let unrelated = scenario.project.path().join("unrelated.crn");
    fs::write(
        &unrelated,
        "let unrelated = mock.test.resource { name = 'unrelated' comment = 'before' }\n",
    )
    .unwrap();
    let case = format!("m1_{shape}_{}", if fail { "failed_apply" } else { "apply" });
    let mut measurement = Trace::new(&case, &scenario);
    let init = measurement.command("init", &["init", "."], None);
    measurement.assert_exit(init, true);
    let initial = measurement.command(
        "initial apply",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        None,
    );
    measurement.assert_exit(initial, true);
    let before = fs::read_to_string(&unrelated).unwrap();
    fs::write(&unrelated, before.replace("'before'", "'after'")).unwrap();
    measurement.snapshot("edit unrelated.comment; sink desired unchanged");
    let second = measurement.command(
        "second apply; sink carried",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        fail.then_some("test.resource.unrelated"),
    );
    measurement.assert_exit(second, !fail);
    assert!(
        measurement.steps[second]["planned_updates"]
            == serde_json::json!(["~ mock.test.resource unrelated"]),
        "unrelated update must run with no sink update:\n{}",
        measurement.render()
    );
    assert!(
        measurement.protected(),
        "{case}: carried write must keep the hash and exclude the marker:\n{}",
        measurement.render()
    );
    scenario.assert_plan_artifacts_do_not_expose_old_material();
}

#[test]
fn apply_carried_retained_secret_at_root_keeps_previous_hash() {
    assert_carried_apply_protected("root", false);
}

#[test]
fn apply_carried_retained_secret_in_module_keeps_previous_hash() {
    assert_carried_apply_protected("module", false);
}

#[test]
fn apply_carried_segmented_secret_reference_keeps_previous_hash() {
    assert_carried_apply_protected("segmented", false);
}

#[test]
fn failed_apply_carried_retained_secret_at_root_keeps_previous_hash() {
    assert_carried_apply_protected("root", true);
}

#[test]
fn failed_apply_carried_retained_secret_in_module_keeps_previous_hash() {
    assert_carried_apply_protected("module", true);
}

#[test]
fn failed_apply_carried_segmented_secret_reference_keeps_previous_hash() {
    assert_carried_apply_protected("segmented", true);
}

fn edit_sink_secret_to_public(scenario: &Scenario) {
    let source = fs::read_to_string(&scenario.source).unwrap();
    let secret = format!("computed = secret('{}')", scenario.marker);
    let (prefix, suffix) = source.rsplit_once(&secret).expect("sink secret expression");
    fs::write(
        &scenario.source,
        format!("{prefix}computed = 'public'{suffix}"),
    )
    .unwrap();
}

fn assert_literal_edit_protected(fail_apply: bool) {
    let scenario = Scenario::new(false, "secret('__MARKER__')");
    let case = if fail_apply {
        "literal_edit_failed_apply"
    } else {
        "literal_edit_refresh"
    };
    let mut measurement = Trace::new(case, &scenario);
    let init = measurement.command("init", &["init", "."], None);
    measurement.assert_exit(init, true);
    let initial = measurement.command(
        "initial apply: sink.computed = secret(marker)",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        None,
    );
    measurement.assert_exit(initial, true);
    edit_sink_secret_to_public(&scenario);
    measurement.snapshot("edit sink.computed to literal public; provider still holds old secret");
    let last = if fail_apply {
        measurement.command(
            "failed update apply before any accepted public write",
            &["apply", "--auto-approve", "--parallelism=1", "."],
            Some("test.resource.sink"),
        )
    } else {
        measurement.command(
            "state refresh before apply",
            &["state", "refresh", "."],
            None,
        )
    };
    measurement.assert_exit(last, !fail_apply);
    assert!(
        measurement.protected(),
        "literal edit before an accepted public write must keep the old secret hashed:\n{}",
        measurement.render()
    );
    scenario.assert_hash(&scenario.state(), "sink", &scenario.marker);
    scenario.assert_plan_artifacts_hide_old_material(true);
    let cached = scenario.run(&["plan", "--refresh=false", "."]);
    assert!(
        cached.contains("(secret)"),
        "stored side must remain masked: {cached}"
    );
}

#[test]
fn refresh_after_literal_edit_keeps_old_secret_hashed() {
    assert_literal_edit_protected(false);
}

#[test]
fn failed_apply_after_literal_edit_keeps_old_secret_hashed() {
    assert_literal_edit_protected(true);
}

#[test]
fn ignored_secret_literal_edit_during_unrelated_update_keeps_previous_hash() {
    let scenario = Scenario::new(false, "secret('__MARKER__')");
    let mut trace = Trace::new("ignored_top_level", &scenario);
    let init = trace.command("init", &["init", "."], None);
    trace.assert_exit(init, true);
    let initial = trace.command(
        "initial secret apply",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        None,
    );
    trace.assert_exit(initial, true);
    edit_sink_secret_to_public(&scenario);
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(
        &scenario.source,
        source.replace(
            "name = 'sink'",
            "name = 'sink'\n  comment = 'after'\n  directives { ignore_changes = [computed] }",
        ),
    )
    .unwrap();
    trace.snapshot("edit ignored computed to public and update comment");
    let apply = trace.command(
        "apply sends only comment",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        None,
    );
    trace.assert_exit(apply, true);
    assert!(
        trace.protected(),
        "ignored computed must not release via another accepted key:\n{}",
        trace.render()
    );
    scenario.assert_hash(&scenario.state(), "sink", &scenario.marker);
    scenario.assert_plan_artifacts_do_not_expose_old_material();
}

#[test]
fn public_literal_equal_to_provider_converges_without_perpetual_update() {
    let scenario = Scenario::new(false, "secret('__MARKER__')");
    let mut measurement = Trace::new("public_literal_equal_to_provider", &scenario);
    let init = measurement.command("init", &["init", "."], None);
    measurement.assert_exit(init, true);
    let initial = measurement.command(
        "initial secret apply",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        None,
    );
    measurement.assert_exit(initial, true);
    edit_sink_secret_to_public(&scenario);
    scenario.set_provider_computed("sink", "public");
    measurement.snapshot("edit desired and provider to the same public literal");
    for (label, args) in [
        ("refresh", vec!["state", "refresh", "."]),
        ("plan", vec!["plan", "."]),
        ("plan --refresh=false", vec!["plan", "--refresh=false", "."]),
        (
            "apply",
            vec!["apply", "--auto-approve", "--parallelism=1", "."],
        ),
        ("final plan", vec!["plan", "."]),
        (
            "final plan --refresh=false",
            vec!["plan", "--refresh=false", "."],
        ),
    ] {
        let step = measurement.command(label, &args, None);
        measurement.assert_exit(step, true);
    }
    assert!(measurement.no_marker(), "{}", measurement.render());
    for step in 3..=8 {
        assert!(
            measurement.steps[step]["sink_hash"] == false,
            "equal public value must release at the first refresh:\n{}",
            measurement.render()
        );
    }
    for step in 4..=8 {
        assert!(
            measurement.steps[step]["planned_changes"] == false,
            "both plan forms and no-op apply must converge:\n{}",
            measurement.render()
        );
    }
    assert!(scenario.computed(&scenario.state(), "sink") == "public");
    scenario.assert_plan_artifacts_do_not_expose_old_material();
}

#[test]
fn public_literal_case_variant_pins_builtin_mock_normalization_behavior() {
    // The built-in test.resource normalizer is a no-op. This pins its closest
    // expressible case: a case-only provider difference, followed by an apply.
    let mut scenario = Scenario::new(false, "secret('__MARKER__')");
    let public = "public";
    let old = public.to_ascii_uppercase();
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(&scenario.source, source.replace(&scenario.marker, &old)).unwrap();
    scenario.marker = old;
    let mut measurement = Trace::new("public_literal_normalization", &scenario);
    let init = measurement.command("init", &["init", "."], None);
    measurement.assert_exit(init, true);
    let initial = measurement.command(
        "initial secret apply",
        &["apply", "--auto-approve", "--parallelism=1", "."],
        None,
    );
    measurement.assert_exit(initial, true);
    edit_sink_secret_to_public(&scenario);
    measurement.snapshot("desired public literal differs from provider only by case");
    for (label, args) in [
        ("refresh", vec!["state", "refresh", "."]),
        ("plan", vec!["plan", "."]),
        ("plan --refresh=false", vec!["plan", "--refresh=false", "."]),
        (
            "apply",
            vec!["apply", "--auto-approve", "--parallelism=1", "."],
        ),
        ("final plan", vec!["plan", "."]),
        (
            "final plan --refresh=false",
            vec!["plan", "--refresh=false", "."],
        ),
    ] {
        let step = measurement.command(label, &args, None);
        measurement.assert_exit(step, true);
    }
    assert!(
        measurement.no_marker(),
        "old value must never be exposed:\n{}",
        measurement.render()
    );
    assert!(
        measurement.steps[3]["sink_hash"] == true,
        "{}",
        measurement.render()
    );
    for step in [4, 5, 6] {
        assert!(
            measurement.steps[step]["planned_updates"]
                == serde_json::json!(["~ mock.test.resource sink"]),
            "the built-in mock does not normalize this case difference:\n{}",
            measurement.render()
        );
    }
    for step in [7, 8] {
        assert!(
            measurement.steps[step]["planned_changes"] == false,
            "normalization measurement must converge:\n{}",
            measurement.render()
        );
    }
    assert!(scenario.computed(&scenario.state(), "sink") == public);
    scenario.assert_plan_artifacts_do_not_expose_old_material();
}

#[test]
fn refresh_equal_public_literal_reports_hash_release_as_update() {
    let scenario = Scenario::new(false, "secret('public')");
    scenario.apply_and_plan("public");
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(
        &scenario.source,
        source.replace("secret('public')", "'public'"),
    )
    .unwrap();
    let output = scenario.run(&["state", "refresh", "."]);
    assert!(scenario.computed(&scenario.state(), "sink") == "public");
    assert!(
        output.lines().any(|line| line.contains("~ computed:")
            && line.contains("(secret)")
            && line.contains("public")),
        "hash release must appear in the refresh change line:\n{output}"
    );
    assert!(
        output.contains("State refreshed: 1 resource updated, 1 resource unchanged."),
        "hash release counts as an updated resource:\n{output}"
    );
    scenario.assert_plan_artifacts_hide_old_material(false);
}

#[test]
fn public_reference_equal_to_provider_converges_at_refresh() {
    let scenario = Scenario::new(false, RETAINED_CALL);
    scenario.apply_and_plan(&format!("prefix-{}", scenario.marker));
    let state = scenario.state();
    let identifier = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "src")
        .unwrap()["current"]["attributes"]["identifier"]
        .as_str()
        .unwrap();
    let source = fs::read_to_string(&scenario.source).unwrap();
    fs::write(
        &scenario.source,
        source.replace(RETAINED_CALL, "src.identifier"),
    )
    .unwrap();
    scenario.set_provider_computed("sink", identifier);
    for args in [
        vec!["state", "refresh", "."],
        vec!["plan", "."],
        vec!["plan", "--refresh=false", "."],
        vec!["apply", "--auto-approve", "--parallelism=1", "."],
        vec!["plan", "."],
        vec!["plan", "--refresh=false", "."],
    ] {
        let output = scenario.run(&args);
        if args[0] == "plan" || args[0] == "apply" {
            assert!(
                output.contains("No changes"),
                "public reference must converge: {output}"
            );
        }
        assert!(
            scenario.computed(&scenario.state(), "sink") == identifier,
            "final public desired must release at refresh"
        );
        assert!(
            !serde_json::to_string(&scenario.state())
                .unwrap()
                .contains(&scenario.marker),
            "state must hide old secret material"
        );
    }
    scenario.assert_plan_artifacts_hide_old_material(false);
}

#[test]
fn refresh_shadow_preparation_does_not_duplicate_pipeline_warnings() {
    let scenario = Scenario::new(false, "secret('__MARKER__')");
    scenario.apply_and_plan(&scenario.marker);
    let mut source = fs::read_to_string(&scenario.source).unwrap();
    source.push_str(
        "\nlet extras = for unused in ['public'] { mock.test.resource { name = 'extra' } }\n",
    );
    fs::write(&scenario.source, source).unwrap();
    let output = scenario.run(&["state", "refresh"]);
    assert_eq!(
        output
            .matches("for-loop binding 'unused' is unused")
            .count(),
        1,
        "refresh must print the same single authored warning as main:\n{output}"
    );
    scenario.assert_hash(&scenario.state(), "sink", &scenario.marker);
    scenario.assert_plan_artifacts_do_not_expose_old_material();
}

#[test]
fn refresh_shadow_preparation_does_not_duplicate_provider_type_warnings() {
    let scenario = Scenario::new(false, "secret('__MARKER__')");
    scenario.apply_and_plan(&scenario.marker);
    let mut source = fs::read_to_string(&scenario.source).unwrap();
    // An unavailable, unused provider leaves the Arn checks deferred. Main
    // emits one diagnostic for each site, then rejects the missing provider.
    source.push_str(
        "\nprovider broken { source = 'file:///definitely-missing/retained-secret.wasm' }\nfn f(x: Arn) { x }\nlet a = f('public')\n",
    );
    fs::write(&scenario.source, source).unwrap();
    let output = scenario.command(&["state", "refresh"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let masked =
        format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&scenario.marker, "<MASKED>");
    assert_eq!(output.status.code(), Some(1), "{masked}");
    assert!(
        !stdout.contains(&scenario.marker) && !stderr.contains(&scenario.marker),
        "{masked}"
    );
    assert_eq!(
        masked
            .matches("was deferred because provider schemas were not loaded")
            .count(),
        2,
        "refresh must emit the same two provider-type diagnostics as main:\n{masked}"
    );
    scenario.assert_hash(&scenario.state(), "sink", &scenario.marker);
    for args in [
        vec!["plan", "--json", "."],
        vec!["plan", "--refresh=false", "--json", "."],
        vec!["plan", "--out", "warning.plan.json", "."],
    ] {
        let output = scenario.command(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let masked =
            format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&scenario.marker, "<MASKED>");
        assert_eq!(output.status.code(), Some(1), "{masked}");
        assert!(
            !stdout.contains(&scenario.marker) && !stderr.contains(&scenario.marker),
            "{masked}"
        );
    }
    let saved = scenario.project.path().join("warning.plan.json");
    if saved.exists() {
        assert!(
            !fs::read_to_string(saved)
                .unwrap()
                .contains(&scenario.marker)
        );
    }
}
