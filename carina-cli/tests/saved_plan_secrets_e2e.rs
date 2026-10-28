//! Saved-plan secret rehydration regressions (carina#3861 / carina#2674).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

use carina_core::resource::{ConcreteValue, DeferredValue, RedactedSecretHash, Value as CoreValue};
use carina_core::value::{PERSISTED_SECRET_KEY, PersistedValue};

const V2_SECRET_PREFIX: &str = "$argon2id$";

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new(main: &str) -> Self {
        Self::new_with_env(main, &[])
    }

    fn new_with_env(main: &str, env: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        fs::write(project.join("main.crn"), main).expect("write main.crn");
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        assert_success(
            "carina init",
            &scenario.carina_with_env(&["init", "."], env),
        );
        scenario
    }

    fn write_main(&self, main: &str) {
        fs::write(self.project.join("main.crn"), main).expect("rewrite main.crn");
    }

    fn write_root_file(&self, name: &str, source: &str) {
        fs::write(self.project.join(name), source).expect("write root configuration file");
    }

    fn write_module(&self, name: &str, source: &str) {
        let directory = self.project.join(name);
        fs::create_dir_all(&directory).expect("create module directory");
        fs::write(directory.join("main.crn"), source).expect("write module main.crn");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args);
        command
    }

    fn carina(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run carina")
    }

    fn carina_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        command.output().expect("run carina with environment")
    }

    fn plan(&self) -> Output {
        self.carina(&["plan", "--out", "plan.json", "."])
    }

    fn apply_plan(&self) -> Output {
        self.carina(&["apply", "--auto-approve", "--lock=false", "plan.json"])
    }

    fn plan_with_env(&self, env: &[(&str, &str)]) -> Output {
        self.carina_with_env(&["plan", "--out", "plan.json", "."], env)
    }

    fn apply_plan_with_env(&self, env: &[(&str, &str)]) -> Output {
        self.carina_with_env(
            &["apply", "--auto-approve", "--lock=false", "plan.json"],
            env,
        )
    }

    fn plan_text(&self) -> String {
        fs::read_to_string(self.project.join("plan.json")).expect("read saved plan")
    }

    fn state_json(&self) -> Value {
        read_json(&self.project.join("carina.state.json"))
    }

    fn provider_json(&self) -> Value {
        read_json(&self.mock_state)
    }
}

const ISSUE_3974_ENV: &str = "SAVED_PLAN_BUILTIN_SECRET_3974";

fn issue_3974_scenario(label: &str, expression: &str, env_value: &str) -> Scenario {
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
        &[(ISSUE_3974_ENV, env_value)],
    );
    scenario.write_root_file(
        "resources.crn",
        &format!(
            r#"let origin = mock.test.resource {{ name = "{label}-origin" }}

let dependent = mock.test.resource {{
  name    = "{label}-dependent"
  comment = {expression}
}}
"#,
        ),
    );
    scenario
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_output_has_no_plaintext(label: &str, output: &Output, plaintexts: &[&str]) {
    let rendered = output_text(output);
    for plaintext in plaintexts {
        assert!(
            !rendered.contains(plaintext),
            "{label} leaked secret plaintext {plaintext:?}:\n{rendered}",
        );
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_secret_verification_rejected_without_mutation(
    scenario: &Scenario,
    output: &Output,
    plaintexts: &[&str],
    expected_message: &str,
    state_before: Option<Vec<u8>>,
    provider_before: Option<Vec<u8>>,
) {
    let rendered = output_text(output);
    assert!(
        !output.status.success(),
        "saved-plan secret verification unexpectedly succeeded:\n{rendered}",
    );
    assert!(
        rendered.contains(expected_message) && rendered.contains("carina plan"),
        "saved-plan secret verification reported the wrong cause:\n{rendered}",
    );
    assert_eq!(
        rendered.matches(expected_message).count(),
        1,
        "saved-plan secret reason was rendered more than once:\n{rendered}",
    );
    assert_output_has_no_plaintext("rejected saved-plan apply", output, plaintexts);
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state changed despite failed secret verification",
    );
    assert_eq!(
        fs::read(&scenario.mock_state).ok(),
        provider_before,
        "provider state changed despite failed secret verification",
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read JSON file"))
        .expect("parse JSON file")
}

fn count_redacted_secrets(value: &Value) -> usize {
    match value {
        Value::Array(values) => values.iter().map(count_redacted_secrets).sum(),
        Value::Object(values) => {
            usize::from(values.contains_key("RedactedSecret"))
                + values.values().map(count_redacted_secrets).sum::<usize>()
        }
        _ => 0,
    }
}

fn assert_saved_plan_has_no_untyped_secret_encoding(value: &Value) {
    fn walk(value: &Value, path: &mut Vec<String>, digest_allowed: bool) {
        match value {
            Value::String(value) => {
                assert!(
                    !value.contains(V2_SECRET_PREFIX) || digest_allowed,
                    "saved plan contains an Argon2id digest outside a typed RedactedSecret hash at {}",
                    path.join("."),
                );
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    path.push(index.to_string());
                    walk(value, path, false);
                    path.pop();
                }
            }
            Value::Object(values) => {
                let is_persisted_secret_tag = values.len() == 1
                    && values
                        .get(PERSISTED_SECRET_KEY)
                        .is_some_and(Value::is_string);
                assert!(
                    !is_persisted_secret_tag,
                    "saved plan contains a persisted-state {PERSISTED_SECRET_KEY} tag at {}",
                    path.join("."),
                );
                let inside_redacted_secret = path
                    .last()
                    .is_some_and(|segment| segment == "RedactedSecret");
                for (key, value) in values {
                    assert!(
                        !key.contains(V2_SECRET_PREFIX),
                        "saved plan contains an Argon2id digest in an object key at {}",
                        path.join("."),
                    );
                    path.push(key.clone());
                    walk(value, path, inside_redacted_secret && key == "hash");
                    path.pop();
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    walk(value, &mut Vec::new(), false);
}

fn assert_plan_is_redacted(scenario: &Scenario, plaintexts: &[&str]) {
    let text = scenario.plan_text();
    for plaintext in plaintexts {
        assert!(
            !text.contains(plaintext),
            "saved plan leaked secret plaintext {plaintext:?}:\n{text}",
        );
    }
    let json: Value = serde_json::from_str(&text).expect("valid saved-plan JSON");
    assert_saved_plan_has_no_untyped_secret_encoding(&json);
    assert!(
        count_redacted_secrets(&json) > 0,
        "saved plan must use a typed RedactedSecret placeholder:\n{text}",
    );
}

fn provider_resource_by_name<'a>(provider: &'a Value, name: &str) -> &'a Value {
    provider_resources(provider)
        .values()
        .find(|attributes| attributes.get("name") == Some(&Value::String(name.to_string())))
        .unwrap_or_else(|| panic!("provider state has no resource named {name}: {provider:#}"))
}

fn provider_resources(provider: &Value) -> &serde_json::Map<String, Value> {
    provider
        .get("states")
        .unwrap_or(provider)
        .as_object()
        .expect("provider state object")
}

fn provider_resource_by_name_mut<'a>(provider: &'a mut Value, name: &str) -> &'a mut Value {
    provider_resources_mut(provider)
        .values_mut()
        .find(|attributes| attributes.get("name") == Some(&Value::String(name.to_string())))
        .unwrap_or_else(|| panic!("provider state has no resource named {name}"))
}

fn provider_resources_mut(provider: &mut Value) -> &mut serde_json::Map<String, Value> {
    if provider.get("states").is_some() {
        provider
            .get_mut("states")
            .and_then(Value::as_object_mut)
            .expect("versioned provider state object")
    } else {
        provider.as_object_mut().expect("provider state object")
    }
}

fn state_resource_by_name<'a>(state: &'a Value, name: &str) -> &'a Value {
    let resource = state["resources"]
        .as_array()
        .expect("state resources array")
        .iter()
        .find(|resource| {
            resource.pointer("/current/attributes/name") == Some(&Value::String(name.to_string()))
        })
        .unwrap_or_else(|| panic!("Carina state has no resource named {name}: {state:#}"));
    &resource["current"]
}

fn plan_resource_by_name<'a>(plan: &'a Value, name: &str) -> &'a Value {
    plan["sorted_resources"]
        .as_array()
        .expect("plan sorted_resources array")
        .iter()
        .find(|resource| {
            resource.pointer("/attributes/name/String") == Some(&Value::String(name.to_string()))
        })
        .unwrap_or_else(|| panic!("saved plan has no resource named {name}: {plan:#}"))
}

fn plan_comment_placeholder_hash<'a>(plan: &'a Value, name: &str) -> &'a str {
    plan_resource_by_name(plan, name)
        .pointer("/attributes/comment/RedactedSecret/hash")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("saved plan has no comment placeholder for {name}: {plan:#}"))
}

fn provider_attribute_placeholder_hash<'a>(plan: &'a Value, key: &str) -> &'a str {
    plan.pointer(&format!(
        "/provider_configs/0/attributes/{key}/RedactedSecret/hash"
    ))
    .and_then(Value::as_str)
    .unwrap_or_else(|| panic!("saved plan has no provider placeholder for {key}: {plan:#}"))
}

fn contains_redacted_secret_hash(value: &Value, expected: &str) -> bool {
    match value {
        Value::Array(values) => values
            .iter()
            .any(|value| contains_redacted_secret_hash(value, expected)),
        Value::Object(values) => {
            values
                .get("RedactedSecret")
                .and_then(|placeholder| placeholder.get("hash"))
                .and_then(Value::as_str)
                == Some(expected)
                || values
                    .values()
                    .any(|value| contains_redacted_secret_hash(value, expected))
        }
        _ => false,
    }
}

fn state_comment_hash<'a>(state: &'a Value, name: &str) -> &'a str {
    state_resource_by_name(state, name)["attributes"]["comment"]
        .get(PERSISTED_SECRET_KEY)
        .and_then(Value::as_str)
        .expect("structural state secret hash")
}

fn current_state_comment_placeholder_hash<'a>(plan: &'a Value, identity: &str) -> &'a str {
    plan["current_states"]
        .as_array()
        .expect("plan current_states array")
        .iter()
        .find(|entry| entry.pointer("/id/identity").and_then(Value::as_str) == Some(identity))
        .and_then(|entry| {
            entry
                .pointer("/current/state/attributes/comment/RedactedSecret/hash")
                .and_then(Value::as_str)
        })
        .unwrap_or_else(|| {
            panic!("saved plan has no current-state comment placeholder for {identity}: {plan:#}")
        })
}

fn assert_secret_hash(value: &Value) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("secret state value is an object, got {value}"));
    assert_eq!(object.len(), 1, "secret tag must contain exactly one entry");
    let hash = object
        .get(PERSISTED_SECRET_KEY)
        .and_then(Value::as_str)
        .expect("secret tag must carry a digest");
    assert!(
        hash.starts_with(V2_SECRET_PREFIX),
        "state must contain a secret hash, got {hash:?}",
    );
    assert!(
        RedactedSecretHash::from_encoded_digest(hash).is_some(),
        "state secret hash must be a validated persisted digest",
    );
}

fn valid_legacy_marker_shaped_string(plaintext: &str) -> String {
    let secret = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
        ConcreteValue::String(plaintext.into()),
    ))));
    let encoded = PersistedValue::encode_provider_value(&secret, None)
        .expect("test secret must be persistable");
    format!(
        "_secret:{}",
        encoded
            .as_json()
            .as_object()
            .and_then(|tag| tag.get(PERSISTED_SECRET_KEY))
            .and_then(Value::as_str)
            .expect("structural test secret carries a digest")
    )
}

#[test]
fn live_apply_canonicalizes_repeated_block_keys_before_provider_and_state_writeback() {
    const ENV: &[(&str, &str)] = &[("SECRET_V", "topsecret-plain-42")];
    const PLAINTEXT: &str = "topsecret-plain-42";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let r = mock.test.resource { name = "seed" }
let b = mock.test.resource {
  name = "b"
  rule { action = r.identifier }
  rule { action = secret(env("SECRET_V")) }
}
"#,
        ENV,
    );

    let apply = scenario.carina_with_env(&["apply", "--auto-approve", "--lock=false", "."], ENV);
    assert_success("live apply with repeated blocks", &apply);
    assert_output_has_no_plaintext("live apply with repeated blocks", &apply, &[PLAINTEXT]);

    let state_text =
        fs::read_to_string(scenario.project.join("carina.state.json")).expect("read Carina state");
    assert!(
        !state_text.contains(PLAINTEXT),
        "Carina state leaked repeated-block secret plaintext:\n{state_text}",
    );
    let state = scenario.state_json();
    let attributes = state_resource_by_name(&state, "b")["attributes"]
        .as_object()
        .expect("resource attributes");
    assert!(
        !attributes.contains_key("rule"),
        "surface block-name key must not survive state writeback: {attributes:#?}",
    );
    let rules = attributes["rules"]
        .as_array()
        .expect("canonical rules list");
    assert_eq!(rules[0]["action"], "seed-id");
    assert_secret_hash(&rules[1]["action"]);

    let provider = scenario.provider_json();
    let provider_attributes = provider_resource_by_name(&provider, "b")
        .as_object()
        .expect("provider resource attributes");
    assert!(
        !provider_attributes.contains_key("rule"),
        "surface block-name key crossed the provider boundary: {provider_attributes:#?}",
    );
}

#[test]
fn block_alias_resource_reference_is_canonical_for_live_and_saved_plan_apply() {
    const SOURCE: &str = r#"backend local { path = "carina.state.json" }
provider mock {}

let r = mock.test.resource {
  name = "seed"
  rules = [{ action = "a" }]
}
let b = mock.test.resource {
  name = "b"
  rule = r.rules
}
"#;

    let assert_canonical_state = |scenario: &Scenario, label: &str| {
        let state = scenario.state_json();
        let attributes = state_resource_by_name(&state, "b")["attributes"]
            .as_object()
            .expect("resource attributes");
        assert!(
            !attributes.contains_key("rule"),
            "{label} kept the block-name alias in Carina state: {attributes:#?}",
        );
        assert_eq!(attributes["rules"][0]["action"], "a");

        let provider = scenario.provider_json();
        let provider_attributes = provider_resource_by_name(&provider, "b")
            .as_object()
            .expect("provider attributes");
        assert!(
            !provider_attributes.contains_key("rule"),
            "{label} sent the block-name alias across the provider boundary: {provider_attributes:#?}",
        );
        assert_eq!(provider_attributes["rules"][0]["action"], "a");
    };

    let live = Scenario::new(SOURCE);
    assert_success("live resource-ref alias plan", &live.carina(&["plan", "."]));
    assert_success(
        "live resource-ref alias apply",
        &live.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    assert_canonical_state(&live, "live apply");

    let saved = Scenario::new(SOURCE);
    assert_success("saved resource-ref alias plan", &saved.plan());
    let saved_plan = read_json(&saved.project.join("plan.json"));
    let unresolved_attributes = saved_plan["unresolved_resources"]
        .as_array()
        .expect("saved-plan unresolved resources")
        .iter()
        .find(|resource| {
            resource.pointer("/attributes/name/String") == Some(&Value::String("b".into()))
        })
        .and_then(|resource| resource["attributes"].as_object())
        .expect("saved-plan unresolved attributes for b");
    assert!(
        unresolved_attributes.contains_key("rules") && !unresolved_attributes.contains_key("rule"),
        "saved plan kept the block-name alias: {unresolved_attributes:#?}",
    );
    assert_success("saved resource-ref alias apply", &saved.apply_plan());
    assert_canonical_state(&saved, "saved-plan apply");
}

#[test]
fn singular_field_matching_another_fields_block_name_is_not_renamed() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name = "transition-assignment"
  lifecycle_configuration = {
    transition = { storage_class = "GLACIER" }
  }
}
"#,
    );

    let apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("apply with singular transition assignment", &apply);

    let state = scenario.state_json();
    let provider = scenario.provider_json();
    for (label, attributes) in [
        (
            "Carina state",
            &state_resource_by_name(&state, "transition-assignment")["attributes"],
        ),
        (
            "mock provider state",
            provider_resource_by_name(&provider, "transition-assignment"),
        ),
    ] {
        let lifecycle = &attributes["lifecycle_configuration"];
        assert_eq!(lifecycle["transition"]["storage_class"], "GLACIER");
        assert!(
            lifecycle.get("transitions").is_none(),
            "{label} incorrectly renamed the singular field: {lifecycle:#?}",
        );
    }
}

#[test]
fn block_syntax_matching_singular_field_uses_list_alias() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name = "transition-blocks"
  lifecycle_configuration = {
    transition { storage_class = "STANDARD_IA" }
    transition { storage_class = "GLACIER" }
  }
}
"#,
    );

    assert_success(
        "plan with ambiguous transition block syntax",
        &scenario.carina(&["plan", "."]),
    );
    let apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("apply with ambiguous transition block syntax", &apply);

    let state = scenario.state_json();
    let provider = scenario.provider_json();
    for (label, attributes) in [
        (
            "Carina state",
            &state_resource_by_name(&state, "transition-blocks")["attributes"],
        ),
        (
            "mock provider state",
            provider_resource_by_name(&provider, "transition-blocks"),
        ),
    ] {
        let lifecycle = &attributes["lifecycle_configuration"];
        let transitions = lifecycle["transitions"]
            .as_array()
            .expect("canonical transitions list");
        assert_eq!(transitions.len(), 2, "{label}: {lifecycle:#?}");
        assert_eq!(transitions[0]["storage_class"], "STANDARD_IA");
        assert_eq!(transitions[1]["storage_class"], "GLACIER");
        assert!(
            lifecycle.get("transition").is_none(),
            "{label} kept the ambiguous block spelling: {lifecycle:#?}",
        );
    }
}

#[test]
fn unknown_block_name_keeps_main_validation_diagnostic() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(
        temp.path().join("main.crn"),
        "provider mock {}\nlet item = mock.test.resource {\n  rul { action = \"allow\" }\n}\n",
    )
    .expect("write main.crn");
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(temp.path())
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["validate", "."])
        .output()
        .expect("run carina validate");
    let rendered = output_text(&output);

    assert!(
        !output.status.success(),
        "unknown block unexpectedly validated"
    );
    assert!(
        rendered.contains("Unknown attribute 'rul', did you mean 'rule'?"),
        "{rendered}"
    );
}

#[test]
fn canonical_attribute_name_keeps_working_as_block_syntax() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

mock.test.resource {
  name = "canonical-block"
  rules { action = "allow" }
}
"#,
    );

    assert_success(
        "canonical-name block validate",
        &scenario.carina(&["validate", "."]),
    );
    assert_success("canonical-name block plan", &scenario.plan());
}

#[test]
fn saved_plan_repeated_block_secret_applies_masked_and_converges() {
    const ENV: &[(&str, &str)] = &[("SECRET_V", "topsecret-plain-42")];
    const PLAINTEXT: &str = "topsecret-plain-42";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let t = mock.test.resource {
  name = "t"
  rule { action = secret(env("SECRET_V")) }
}
"#,
        ENV,
    );

    let plan = scenario.plan_with_env(ENV);
    assert_success("saved plan with repeated-block secret", &plan);
    assert_output_has_no_plaintext("saved plan with repeated-block secret", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);
    let saved_plan = read_json(&scenario.project.join("plan.json"));
    let origin_attributes = saved_plan["unresolved_resources"]
        .as_array()
        .expect("saved-plan unresolved resources")
        .iter()
        .find(|resource| {
            resource.pointer("/attributes/name/String") == Some(&Value::String("t".to_string()))
        })
        .and_then(|resource| resource["attributes"].as_object())
        .expect("saved-plan origin attributes for t");
    assert!(
        !origin_attributes.contains_key("rule") && origin_attributes.contains_key("rules"),
        "saved-plan secret positions must use the canonical attribute name: {origin_attributes:#?}",
    );

    let apply = scenario.apply_plan_with_env(ENV);
    assert_success("saved-plan apply with repeated-block secret", &apply);
    assert_output_has_no_plaintext(
        "saved-plan apply with repeated-block secret",
        &apply,
        &[PLAINTEXT],
    );

    let state_text =
        fs::read_to_string(scenario.project.join("carina.state.json")).expect("read Carina state");
    assert!(
        !state_text.contains(PLAINTEXT),
        "Carina state leaked repeated-block secret plaintext:\n{state_text}",
    );
    let state = scenario.state_json();
    let attributes = state_resource_by_name(&state, "t")["attributes"]
        .as_object()
        .expect("resource attributes");
    assert!(
        !attributes.contains_key("rule"),
        "surface block-name key must not survive saved-plan state writeback: {attributes:#?}",
    );
    let rules = attributes["rules"]
        .as_array()
        .expect("canonical rules list");
    assert_secret_hash(&rules[0]["action"]);

    let second_plan = scenario.carina_with_env(&["plan", "."], ENV);
    assert_success("second plan after saved-plan apply", &second_plan);
    assert_output_has_no_plaintext(
        "second plan after saved-plan apply",
        &second_plan,
        &[PLAINTEXT],
    );
    assert!(
        output_text(&second_plan).contains("No changes. Infrastructure is up-to-date."),
        "second plan must be empty after repeated-block saved-plan apply:\n{}",
        output_text(&second_plan),
    );
}

#[test]
fn saved_plan_nested_repeated_block_alias_applies_masked_and_converges() {
    const ENV: &[(&str, &str)] = &[("SECRET_V", "nested-topsecret-3973")];
    const PLAINTEXT: &str = "nested-topsecret-3973";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let t = mock.test.resource {
  name = "nested"
  rule {
    action = "allow"
    condition { value = secret(env("SECRET_V")) }
  }
}
"#,
        ENV,
    );

    let plan = scenario.plan_with_env(ENV);
    assert_success("saved plan with nested repeated-block secret", &plan);
    assert_output_has_no_plaintext(
        "saved plan with nested repeated-block secret",
        &plan,
        &[PLAINTEXT],
    );
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);

    let apply = scenario.apply_plan_with_env(ENV);
    assert_success("saved-plan apply with nested repeated-block secret", &apply);
    assert_output_has_no_plaintext(
        "saved-plan apply with nested repeated-block secret",
        &apply,
        &[PLAINTEXT],
    );

    let state_text =
        fs::read_to_string(scenario.project.join("carina.state.json")).expect("read Carina state");
    assert!(
        !state_text.contains(PLAINTEXT),
        "Carina state leaked nested repeated-block secret plaintext:\n{state_text}",
    );
    let state = scenario.state_json();
    let attributes = state_resource_by_name(&state, "nested")["attributes"]
        .as_object()
        .expect("resource attributes");
    assert!(
        !attributes.contains_key("rule"),
        "top-level surface block-name key survived state writeback: {attributes:#?}",
    );
    let rule = &attributes["rules"][0];
    assert!(
        rule.get("condition").is_none(),
        "nested surface block-name key survived state writeback: {rule:#?}",
    );
    assert_secret_hash(&rule["conditions"][0]["value"]);

    let provider = scenario.provider_json();
    let provider_rule = &provider_resource_by_name(&provider, "nested")["rules"][0];
    assert!(
        provider_rule.get("condition").is_none(),
        "nested surface block-name key crossed the provider boundary: {provider_rule:#?}",
    );

    let second_plan = scenario.carina_with_env(&["plan", "."], ENV);
    assert_success(
        "second plan after nested-block saved-plan apply",
        &second_plan,
    );
    assert!(
        output_text(&second_plan).contains("No changes. Infrastructure is up-to-date."),
        "second plan must be empty after nested repeated-block apply:\n{}",
        output_text(&second_plan),
    );
}

#[test]
fn marker_shaped_plaintext_stays_plain_through_state_and_saved_plan_apply() {
    let marker = valid_legacy_marker_shaped_string("saved-plan-marker-collision");
    let scenario = Scenario::new(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

mock.test.resource {{
  name    = "marker-collision"
  comment = "{marker}"
  tags = {{
    nested = "{marker}"
  }}
}}
"#
    ));

    assert_success(
        "initial marker-collision apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let provider = scenario.provider_json();
    let provider_resource = provider_resource_by_name(&provider, "marker-collision");
    assert_eq!(provider_resource["comment"], marker);
    assert_eq!(provider_resource["tags"]["nested"], marker);
    let state = scenario.state_json();
    let state_resource = state_resource_by_name(&state, "marker-collision");
    assert_eq!(state_resource["attributes"]["comment"], marker);
    assert_eq!(state_resource["attributes"]["tags"]["nested"], marker);

    assert_success("marker-collision plan --out", &scenario.plan());
    let plan = read_json(&scenario.project.join("plan.json"));
    let desired = plan_resource_by_name(&plan, "marker-collision");
    assert_eq!(
        desired.pointer("/attributes/comment/String"),
        Some(&Value::String(marker.clone()))
    );
    assert_eq!(
        desired.pointer("/attributes/tags/Map/nested/String"),
        Some(&Value::String(marker.clone()))
    );
    let current = plan["current_states"]
        .as_array()
        .expect("saved plan current states")
        .iter()
        .find(|entry| {
            entry
                .pointer("/current/state/attributes/name/String")
                .and_then(Value::as_str)
                == Some("marker-collision")
        })
        .unwrap_or_else(|| panic!("saved plan has no marker-collision current state: {plan:#}"));
    assert_eq!(
        current.pointer("/current/state/attributes/comment/String"),
        Some(&Value::String(marker.clone()))
    );
    assert_eq!(
        current.pointer("/current/state/attributes/tags/Map/nested/String"),
        Some(&Value::String(marker.clone()))
    );
    assert!(
        !contains_redacted_secret_hash(&plan, marker.strip_prefix("_secret:").unwrap()),
        "marker-shaped plaintext must never appear as RedactedSecret: {plan:#}",
    );

    assert_success("marker-collision saved-plan apply", &scenario.apply_plan());
    let provider = scenario.provider_json();
    let provider_resource = provider_resource_by_name(&provider, "marker-collision");
    assert_eq!(provider_resource["comment"], marker);
    assert_eq!(provider_resource["tags"]["nested"], marker);
    let state = scenario.state_json();
    let state_resource = state_resource_by_name(&state, "marker-collision");
    assert_eq!(state_resource["attributes"]["comment"], marker);
    assert_eq!(state_resource["attributes"]["tags"]["nested"], marker);
}

#[test]
fn saved_plan_rehydrates_let_bound_and_anonymous_secrets() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let bound = mock.test.resource {
  name    = "bound"
  comment = secret("BOUND_PLAINTEXT_3861")
}

mock.test.resource {
  name    = "anonymous"
  comment = secret("ANONYMOUS_PLAINTEXT_2674")
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(
        &scenario,
        &["BOUND_PLAINTEXT_3861", "ANONYMOUS_PLAINTEXT_2674"],
    );
    assert_success("carina apply saved plan", &scenario.apply_plan());

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "bound")["comment"],
        "BOUND_PLAINTEXT_3861",
    );
    assert_eq!(
        provider_resource_by_name(&provider, "anonymous")["comment"],
        "ANONYMOUS_PLAINTEXT_2674",
    );

    let state = scenario.state_json();
    assert_secret_hash(&state_resource_by_name(&state, "bound")["attributes"]["comment"]);
    assert_secret_hash(&state_resource_by_name(&state, "anonymous")["attributes"]["comment"]);
}

#[test]
fn saved_plan_secret_digests_are_distinct_and_match_state_writeback() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

mock.test.resource {
  name    = "same-secret-a"
  comment = secret("SHARED_PLAINTEXT_3861")
}

mock.test.resource {
  name    = "same-secret-b"
  comment = secret("SHARED_PLAINTEXT_3861")
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(&scenario, &["SHARED_PLAINTEXT_3861"]);
    let plan = read_json(&scenario.project.join("plan.json"));
    let first_plan_hash = plan_comment_placeholder_hash(&plan, "same-secret-a").to_string();
    let second_plan_hash = plan_comment_placeholder_hash(&plan, "same-secret-b").to_string();
    assert_ne!(
        first_plan_hash, second_plan_hash,
        "the plan must not reveal that two resources share a secret",
    );

    assert_success("carina apply saved plan", &scenario.apply_plan());
    let state = scenario.state_json();
    for (name, plan_hash) in [
        ("same-secret-a", first_plan_hash),
        ("same-secret-b", second_plan_hash),
    ] {
        let state_hash = state_resource_by_name(&state, name)["attributes"]["comment"]
            .get(PERSISTED_SECRET_KEY)
            .and_then(Value::as_str)
            .expect("structural state secret hash");
        assert_eq!(
            plan_hash, state_hash,
            "state writeback must reuse the exact saved-plan digest for {name}",
        );
    }
}

#[test]
fn saved_plan_rehydrates_provider_config_secret_with_independent_digest() {
    const ENV: &[(&str, &str)] = &[(
        "SAVED_PLAN_PROVIDER_TOKEN_3861",
        "SHARED_POSITION_SECRET_3861",
    )];
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {
  token = secret(env("SAVED_PLAN_PROVIDER_TOKEN_3861"))
}

mock.test.resource {
  name    = "provider-secret"
  comment = secret(env("SAVED_PLAN_PROVIDER_TOKEN_3861"))
}
"#,
        ENV,
    );

    assert_success("carina plan --out", &scenario.plan_with_env(ENV));
    assert_plan_is_redacted(&scenario, &["SHARED_POSITION_SECRET_3861"]);
    let plan = read_json(&scenario.project.join("plan.json"));
    let provider_hash = provider_attribute_placeholder_hash(&plan, "token");
    let resource_hash = plan_comment_placeholder_hash(&plan, "provider-secret");
    assert_ne!(
        provider_hash, resource_hash,
        "independent placeholders must use independent random salts",
    );

    assert_success(
        "carina apply saved plan",
        &scenario.apply_plan_with_env(ENV),
    );
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "provider-secret")["comment"],
        "SHARED_POSITION_SECRET_3861",
    );
}

#[test]
fn saved_plan_rejects_changed_provider_config_secret_before_provider_call() {
    const ORIGINAL: &[(&str, &str)] =
        &[("SAVED_PLAN_PROVIDER_ONLY_3861", "ORIGINAL_PROVIDER_3861")];
    const CHANGED: &[(&str, &str)] = &[("SAVED_PLAN_PROVIDER_ONLY_3861", "CHANGED_PROVIDER_3861")];
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {
  token = secret(env("SAVED_PLAN_PROVIDER_ONLY_3861"))
}

mock.test.resource {
  name = "provider-only-secret"
}
"#,
        ORIGINAL,
    );

    assert_success("carina plan --out", &scenario.plan_with_env(ORIGINAL));
    assert_plan_is_redacted(&scenario, &["ORIGINAL_PROVIDER_3861"]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();

    let apply = scenario.apply_plan_with_env(CHANGED);
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "changed provider secret applied:\n{rendered}"
    );
    assert!(
        rendered.contains("secret changed since the plan") && rendered.contains("carina plan"),
        "changed provider secret must produce replan guidance:\n{rendered}",
    );
    assert!(
        !scenario.mock_state.exists(),
        "provider must not be called before provider-config secret verification",
    );
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state must remain unchanged when provider-config verification fails",
    );
}

#[test]
fn saved_plan_rehydrates_nested_and_interpolated_secrets() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let nested = mock.test.resource {
  name = "nested"
  tags = {
    password = secret("NESTED_PLAINTEXT_3861")
  }
}

let interpolated = mock.test.resource {
  name    = "interpolated"
  comment = "prefix-${secret("INTERPOLATED_PLAINTEXT_3861")}-suffix"
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(
        &scenario,
        &["NESTED_PLAINTEXT_3861", "INTERPOLATED_PLAINTEXT_3861"],
    );
    assert_success("carina apply saved plan", &scenario.apply_plan());

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "nested")["tags"]["password"],
        "NESTED_PLAINTEXT_3861",
    );
    assert_eq!(
        provider_resource_by_name(&provider, "interpolated")["comment"],
        "prefix-INTERPOLATED_PLAINTEXT_3861-suffix",
    );

    let state = scenario.state_json();
    assert_secret_hash(&state_resource_by_name(&state, "nested")["attributes"]["tags"]["password"]);
    assert_secret_hash(&state_resource_by_name(&state, "interpolated")["attributes"]["comment"]);
}

#[test]
fn saved_plan_hash_inventory_ignores_runtime_reference_siblings() {
    const PLAINTEXT: &str = "SPARSE_PLACEHOLDER_PLAIN_3863";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let vpc = mock.test.resource {
  name = "runtime-vpc"
}

let consumer = mock.test.resource {
  name = "runtime-consumer"
  tags = {
    S   = secret("SPARSE_PLACEHOLDER_PLAIN_3863")
    Vpc = vpc.identifier
  }
}
"#,
    );

    let plan = scenario.plan();
    assert_success("saved plan with mixed placeholder/ref map", &plan);
    assert_output_has_no_plaintext("mixed placeholder/ref plan", &plan, &[PLAINTEXT]);
    let apply = scenario.apply_plan();
    assert_success("apply mixed placeholder/ref saved plan", &apply);
    assert_output_has_no_plaintext(
        "apply mixed placeholder/ref saved plan",
        &apply,
        &[PLAINTEXT],
    );

    let provider = scenario.provider_json();
    let tags = &provider_resource_by_name(&provider, "runtime-consumer")["tags"];
    assert_eq!(tags["S"], PLAINTEXT);
    assert!(tags["Vpc"].as_str().is_some_and(|value| !value.is_empty()));
    assert_secret_hash(
        &state_resource_by_name(&scenario.state_json(), "runtime-consumer")["attributes"]["tags"]["S"],
    );
}

#[test]
fn saved_plan_rehydrates_secret_interpolation_with_runtime_reference() {
    const PLAINTEXT: &str = "RUNTIME_INTERPOLATION_PLAINX_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let origin = mock.test.resource {
  name = "origin"
}

let dependent = mock.test.resource {
  name    = "dependent"
  comment = "${secret("RUNTIME_INTERPOLATION_PLAINX_3861")}-${origin.identifier}"
}
"#,
    );

    let plan = scenario.plan();
    assert_success("carina plan --out", &plan);
    assert_output_has_no_plaintext("carina plan --out", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);

    let apply = scenario.apply_plan();
    assert_success("carina apply saved plan", &apply);
    assert_output_has_no_plaintext("carina apply saved plan", &apply, &[PLAINTEXT]);

    let provider = scenario.provider_json();
    let comment = provider_resource_by_name(&provider, "dependent")["comment"]
        .as_str()
        .expect("provider comment");
    assert!(
        comment.starts_with(&format!("{PLAINTEXT}-")),
        "provider must receive the resolved interpolation, got {comment:?}",
    );
    assert_secret_hash(
        &state_resource_by_name(&scenario.state_json(), "dependent")["attributes"]["comment"],
    );
}

#[test]
fn saved_plan_rehydrates_secret_inside_builtin_with_runtime_reference() {
    const PLAINTEXT: &str = "builtin_secret_plaintext_3974";
    let scenario = issue_3974_scenario(
        "builtin",
        r#"upper("${origin.identifier}-${secret(env("SAVED_PLAN_BUILTIN_SECRET_3974"))}")"#,
        PLAINTEXT,
    );

    let plan = scenario.plan_with_env(&[(ISSUE_3974_ENV, PLAINTEXT)]);
    assert_success("builtin-wrapped secret saved plan", &plan);
    assert_output_has_no_plaintext("builtin-wrapped secret saved plan", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);

    let apply = scenario.apply_plan_with_env(&[(ISSUE_3974_ENV, PLAINTEXT)]);
    assert_success("apply builtin-wrapped secret saved plan", &apply);
    assert_output_has_no_plaintext(
        "apply builtin-wrapped secret saved plan",
        &apply,
        &[PLAINTEXT],
    );

    let provider = scenario.provider_json();
    let comment = provider_resource_by_name(&provider, "builtin-dependent")["comment"]
        .as_str()
        .expect("provider comment");
    assert!(
        comment.ends_with("-BUILTIN_SECRET_PLAINTEXT_3974"),
        "provider must receive the resolved upper-case secret interpolation, got {comment:?}",
    );
    assert_secret_hash(
        &state_resource_by_name(&scenario.state_json(), "builtin-dependent")["attributes"]["comment"],
    );
}

#[test]
fn saved_plan_rehydrates_secret_inside_builtin_with_deferred_coalesce() {
    const PLAINTEXT: &str = "coalesce_secret_plaintext_3974";
    let scenario = issue_3974_scenario(
        "coalesce",
        r#"upper(origin.comment ?? secret(env("SAVED_PLAN_BUILTIN_SECRET_3974")))"#,
        PLAINTEXT,
    );

    let plan = scenario.plan_with_env(&[(ISSUE_3974_ENV, PLAINTEXT)]);
    assert_success("coalesced builtin secret saved plan", &plan);
    assert_output_has_no_plaintext("coalesced builtin secret saved plan", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);

    let apply = scenario.apply_plan_with_env(&[(ISSUE_3974_ENV, PLAINTEXT)]);
    assert_success("apply coalesced builtin secret saved plan", &apply);
    assert_output_has_no_plaintext(
        "apply coalesced builtin secret saved plan",
        &apply,
        &[PLAINTEXT],
    );

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "coalesce-dependent")["comment"],
        "COALESCE_SECRET_PLAINTEXT_3974",
    );
    assert_secret_hash(
        &state_resource_by_name(&scenario.state_json(), "coalesce-dependent")["attributes"]["comment"],
    );
}

#[test]
fn saved_plan_builtin_wrapped_deferred_secrets_reject_changed_values() {
    const PLANNED: &str = "planned_secret_plaintext_3974";
    const CHANGED: &str = "changed_secret_plaintext_3974";
    for (label, expression) in [
        (
            "changed-interpolation",
            r#"upper("${origin.identifier}-${secret(env("SAVED_PLAN_BUILTIN_SECRET_3974"))}")"#,
        ),
        (
            "changed-coalesce",
            r#"upper(origin.comment ?? secret(env("SAVED_PLAN_BUILTIN_SECRET_3974")))"#,
        ),
    ] {
        let scenario = issue_3974_scenario(label, expression, PLANNED);
        let plan = scenario.plan_with_env(&[(ISSUE_3974_ENV, PLANNED)]);
        assert_success(&format!("{label} saved plan"), &plan);
        assert_plan_is_redacted(&scenario, &[PLANNED]);
        let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
        let provider_before = fs::read(&scenario.mock_state).ok();

        let apply = scenario.apply_plan_with_env(&[(ISSUE_3974_ENV, CHANGED)]);
        assert_secret_verification_rejected_without_mutation(
            &scenario,
            &apply,
            &[PLANNED, CHANGED],
            "secret changed since the plan",
            state_before,
            provider_before,
        );
    }
}

#[test]
fn saved_plan_builtin_wrapped_deferred_secrets_reject_unavailable_values() {
    const PLANNED: &str = "available_secret_plaintext_3974";
    for (label, expression, expected, unexpected) in [
        (
            "unavailable-interpolation",
            r#"upper("${origin.identifier}-${secret(env("SAVED_PLAN_BUILTIN_SECRET_3974"))}")"#,
            "current secret could not be evaluated",
            "current configuration could not be evaluated",
        ),
        (
            "unavailable-coalesce",
            r#"upper(origin.comment ?? secret(env("SAVED_PLAN_BUILTIN_SECRET_3974")))"#,
            "current secret could not be evaluated",
            "current configuration could not be evaluated",
        ),
    ] {
        let scenario = issue_3974_scenario(label, expression, PLANNED);
        let plan = scenario.plan_with_env(&[(ISSUE_3974_ENV, PLANNED)]);
        assert_success(&format!("{label} saved plan"), &plan);
        assert_plan_is_redacted(&scenario, &[PLANNED]);
        let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
        let provider_before = fs::read(&scenario.mock_state).ok();

        let apply = scenario
            .command(&["apply", "--auto-approve", "--lock=false", "plan.json"])
            .env_remove(ISSUE_3974_ENV)
            .output()
            .expect("run saved-plan apply without secret environment variable");
        assert_secret_verification_rejected_without_mutation(
            &scenario,
            &apply,
            &[PLANNED],
            expected,
            state_before,
            provider_before,
        );
        assert!(
            !output_text(&apply).contains(unexpected),
            "{label} used the wrong saved-plan verification error variant: {}",
            output_text(&apply),
        );
    }
}

#[test]
fn saved_plan_unselected_coalesce_fallback_reports_secret_evaluation_unavailable() {
    const PLANNED: &str = "coalesce_unavailable_plaintext_3974";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let r = mock.test.resource { name = "unselected-coalesce-origin" }
let dependent = mock.test.resource {
  name    = "unselected-coalesce-dependent"
  comment = upper(r.comment ?? secret(env("SAVED_PLAN_BUILTIN_SECRET_3974")))
}
"#,
        &[(ISSUE_3974_ENV, PLANNED)],
    );

    let plan = scenario.plan_with_env(&[(ISSUE_3974_ENV, PLANNED)]);
    assert_success("unselected coalesce fallback saved plan", &plan);
    assert_plan_is_redacted(&scenario, &[PLANNED]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    // `r.comment` is still unknown while the saved-plan candidate is built,
    // so the fallback is retained without being selected. Its explicit
    // Unavailable leaf must still win over hash comparison.
    let apply = scenario
        .command(&["apply", "--auto-approve", "--lock=false", "plan.json"])
        .env_remove(ISSUE_3974_ENV)
        .output()
        .expect("run saved-plan apply without coalesce fallback environment variable");
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[PLANNED],
        "current secret could not be evaluated",
        state_before,
        provider_before,
    );
    assert!(
        !output_text(&apply).contains("current configuration could not be evaluated"),
        "an unavailable coalesce fallback must be a per-secret verification error",
    );
    assert!(
        !output_text(&apply).contains("secret changed since the plan"),
        "an unavailable coalesce fallback must not be reported as a hash mismatch",
    );
}

#[test]
fn saved_plan_rehydrates_secret_in_module_expanded_resource() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let secrets = use { source = "./secrets" }
let instance = secrets {
  value = secret("MODULE_PLAINTEXT_3861")
}
"#,
    );
    scenario.write_module(
        "secrets",
        r#"arguments {
  value: String
}

let item = mock.test.resource {
  name    = "module-secret"
  comment = value
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(&scenario, &["MODULE_PLAINTEXT_3861"]);
    assert_success("carina apply saved plan", &scenario.apply_plan());

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "module-secret")["comment"],
        "MODULE_PLAINTEXT_3861",
    );
    let state = scenario.state_json();
    assert_secret_hash(&state_resource_by_name(&state, "module-secret")["attributes"]["comment"]);
}

#[test]
fn saved_plan_composition_secret_uses_call_id_after_equal_width_binding_rename() {
    const ORIGINAL: &str = "ORIGIN_CALL_SECRET_PLAIN_4041";
    const DECOY: &str = "DECOY_CALL_SECRET_PLAIN_4041";
    let scenario = Scenario::new(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let tokens = use {{ source = "./tokens" }}
let alpha = tokens {{
  value = secret("{ORIGINAL}")
}}
"#,
    ));
    scenario.write_module(
        "tokens",
        r#"arguments {
  value: String
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(&scenario, &[ORIGINAL]);

    // `alpha` and `bravo` have equal width, so the authored call retains the
    // same file/span identity. The later `alpha` is a decoy that the removed
    // binding-based join would have selected.
    scenario.write_main(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let tokens = use {{ source = "./tokens" }}
let bravo = tokens {{
  value = secret("{ORIGINAL}")
}}

let alpha = tokens {{
  value = secret("{DECOY}")
}}
"#,
    ));

    let apply = scenario.apply_plan();
    assert_success("carina apply saved plan after call rename", &apply);
    assert_output_has_no_plaintext("saved-plan renamed-call apply", &apply, &[ORIGINAL, DECOY]);
}

#[test]
fn saved_plan_composition_secret_rejects_moved_call_without_name_fallback() {
    const ORIGINAL: &str = "MOVED_ORIGIN_SECRET_PLAIN_4041";
    const DECOY: &str = "MOVED_DECOY_SECRET_PLAIN_4041";
    let scenario = Scenario::new(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let tokens = use {{ source = "./tokens" }}
let alpha = tokens {{
  value = secret("{ORIGINAL}")
}}
"#,
    ));
    scenario.write_module(
        "tokens",
        r#"arguments {
  value: String
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(&scenario, &[ORIGINAL]);

    scenario.write_main(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let tokens = use {{ source = "./tokens" }}


let bravo = tokens {{
  value = secret("{ORIGINAL}")
}}

let alpha = tokens {{
  value = secret("{DECOY}")
}}
"#,
    ));

    let apply = scenario.apply_plan();
    assert!(
        !apply.status.success(),
        "moved call must invalidate the saved plan"
    );
    let rendered = output_text(&apply);
    assert!(
        rendered.contains("composition call id chain main.crn:")
            && rendered.contains("disagreed with the saved plan")
            && rendered.contains("re-run `carina plan --out plan.json "),
        "stale composition error must name the call location and replan command:\n{rendered}",
    );
    assert_output_has_no_plaintext("saved-plan stale-call apply", &apply, &[ORIGINAL, DECOY]);
}

#[test]
fn unrelated_secret_does_not_join_a_moved_secret_free_composition() {
    const SECRET: &str = "UNRELATED_RESOURCE_SECRET_PLAIN_4041";
    let scenario = Scenario::new(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let unrelated = mock.test.resource {{
  name    = "unrelated-secret"
  comment = secret("{SECRET}")
}}

let plain = use {{ source = "./plain" }}
let instance = plain {{
  value = "not-secret"
}}
"#,
    ));
    scenario.write_module(
        "plain",
        r#"arguments {
  value: String
}

let item = mock.test.resource {
  name    = "plain-module"
  comment = value
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(&scenario, &[SECRET]);

    scenario.write_main(&format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let unrelated = mock.test.resource {{
  name    = "unrelated-secret"
  comment = secret("{SECRET}")
}}

let plain = use {{ source = "./plain" }}
// Moving this secret-free call must not require a saved-origin join.
let instance = plain {{
  value = "not-secret"
}}
"#,
    ));

    let apply = scenario.apply_plan();
    assert_success("saved-plan apply after moving secret-free call", &apply);
    assert_output_has_no_plaintext("saved-plan moved secret-free call", &apply, &[SECRET]);
}

#[test]
fn saved_plan_module_repeated_block_secret_is_canonical_masked_and_converges() {
    const ENV: &[(&str, &str)] = &[("MODULE_RULE_SECRET", "module-rule-plain-3973")];
    const PLAINTEXT: &str = "module-rule-plain-3973";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let component = use { source = "./component" }
let instance = component {
  value = secret(env("MODULE_RULE_SECRET"))
}
"#,
        ENV,
    );
    scenario.write_module(
        "component",
        r#"arguments {
  value: String
}

let item = mock.test.resource {
  name = "module-rule-secret"
  rule {
    action = "allow"
    condition { value = value }
  }
}
"#,
    );

    let plan = scenario.plan_with_env(ENV);
    assert_success("module repeated-block plan", &plan);
    assert_output_has_no_plaintext("module repeated-block plan", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);

    let apply = scenario.apply_plan_with_env(ENV);
    assert_success("module repeated-block saved-plan apply", &apply);
    assert_output_has_no_plaintext(
        "module repeated-block saved-plan apply",
        &apply,
        &[PLAINTEXT],
    );

    let state_text =
        fs::read_to_string(scenario.project.join("carina.state.json")).expect("read Carina state");
    assert!(!state_text.contains(PLAINTEXT), "{state_text}");
    let state = scenario.state_json();
    let attributes = state_resource_by_name(&state, "module-rule-secret")["attributes"]
        .as_object()
        .expect("resource attributes");
    assert!(!attributes.contains_key("rule"), "{attributes:#?}");
    let rule = &attributes["rules"][0];
    assert!(rule.get("condition").is_none(), "{rule:#?}");
    assert_secret_hash(&rule["conditions"][0]["value"]);

    let provider = scenario.provider_json();
    let provider_attributes = provider_resource_by_name(&provider, "module-rule-secret")
        .as_object()
        .expect("provider resource attributes");
    assert!(
        !provider_attributes.contains_key("rule"),
        "{provider_attributes:#?}"
    );
    assert!(
        provider_attributes["rules"][0].get("condition").is_none(),
        "{provider_attributes:#?}",
    );

    let second_plan = scenario.carina_with_env(&["plan", "."], ENV);
    assert_success("module repeated-block second plan", &second_plan);
    assert!(
        output_text(&second_plan).contains("No changes. Infrastructure is up-to-date."),
        "{}",
        output_text(&second_plan),
    );
}

#[test]
fn saved_plan_rejects_changed_secret_before_provider_or_state_mutation() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name    = "changed"
  comment = secret(env("SAVED_PLAN_SECRET_3861"))
}
"#,
    );

    let plan = scenario
        .command(&["plan", "--out", "plan.json", "."])
        .env("SAVED_PLAN_SECRET_3861", "ORIGINAL_ENV_PLAINTEXT_3861")
        .output()
        .expect("run carina plan");
    assert_success("carina plan --out", &plan);
    assert_plan_is_redacted(&scenario, &["ORIGINAL_ENV_PLAINTEXT_3861"]);

    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let apply = scenario
        .command(&["apply", "--auto-approve", "--lock=false", "plan.json"])
        .env("SAVED_PLAN_SECRET_3861", "CHANGED_ENV_PLAINTEXT_3861")
        .output()
        .expect("run carina apply");
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "changed secret apply passed:\n{rendered}"
    );
    assert!(
        rendered.contains("secret changed since the plan") && rendered.contains("carina plan"),
        "changed-secret diagnostic must explain how to recover:\n{rendered}",
    );
    assert!(
        !scenario.mock_state.exists(),
        "provider must not be called when rehydration fails: {}",
        scenario.mock_state.display(),
    );
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state must be unchanged when rehydration fails",
    );
}

#[test]
fn saved_plan_rejects_unavailable_secret_with_replan_guidance() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name    = "missing"
  comment = secret(env("SAVED_PLAN_MISSING_SECRET_3861"))
}
"#,
    );

    let plan = scenario
        .command(&["plan", "--out", "plan.json", "."])
        .env(
            "SAVED_PLAN_MISSING_SECRET_3861",
            "AVAILABLE_AT_PLAN_TIME_3861",
        )
        .output()
        .expect("run carina plan");
    assert_success("carina plan --out", &plan);
    assert_plan_is_redacted(&scenario, &["AVAILABLE_AT_PLAN_TIME_3861"]);

    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let apply = scenario
        .command(&["apply", "--auto-approve", "--lock=false", "plan.json"])
        .env_remove("SAVED_PLAN_MISSING_SECRET_3861")
        .output()
        .expect("run carina apply");
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "unavailable secret apply passed:\n{rendered}"
    );
    assert!(
        rendered.contains("could not be evaluated") && rendered.contains("carina plan"),
        "unavailable-secret diagnostic must explain how to recover:\n{rendered}",
    );
    assert_eq!(
        rendered.matches("could not be evaluated").count(),
        1,
        "unavailable-secret reason was rendered more than once:\n{rendered}",
    );
    assert!(
        !scenario.mock_state.exists(),
        "provider must not be called when secret evaluation fails: {}",
        scenario.mock_state.display(),
    );
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state must be unchanged when secret evaluation fails",
    );
}

#[test]
fn saved_plan_configuration_change_reason_and_location_render_once() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let shape_once = mock.test.resource {
  name    = "shape-once"
  comment = secret("SHAPE_ONCE_PLAINTEXT_3890")
}
"#,
    );
    assert_success("shape-change saved plan", &scenario.plan());
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let shape_once = mock.test.resource {
  name = "shape-once"
}
"#,
    );

    let apply = scenario.apply_plan();
    let rendered = output_text(&apply);

    assert!(
        !apply.status.success(),
        "shape-changed plan applied:\n{rendered}"
    );
    assert_eq!(
        rendered
            .matches("recorded secret position no longer exists")
            .count(),
        1,
        "configuration-change reason was not rendered exactly once:\n{rendered}",
    );
    assert_eq!(
        rendered.matches("shape_once").count(),
        1,
        "configuration-change location was rendered more than once:\n{rendered}",
    );
}

#[test]
fn saved_plan_vanished_orphan_reason_and_resource_render_once() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let orphan_once = mock.test.resource {
  name    = "orphan-once"
  comment = secret("ORPHAN_ONCE_PLAINTEXT_3890")
}
"#,
    );
    assert_success(
        "initial orphan fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
    );
    assert_success(
        "refresh-disabled orphan saved plan",
        &scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]),
    );
    fs::write(&scenario.mock_state, b"{}\n").expect("remove orphan from provider state");

    let apply = scenario.apply_plan();
    let rendered = output_text(&apply);

    assert!(
        !apply.status.success(),
        "vanished orphan plan applied:\n{rendered}"
    );
    assert_eq!(
        rendered.matches("orphan resource").count(),
        1,
        "orphan-vanished reason was rendered more than once:\n{rendered}",
    );
    assert_eq!(
        rendered.matches("orphan_once").count(),
        1,
        "orphan resource was rendered more than once:\n{rendered}",
    );
}

#[test]
fn saved_plan_update_sends_plaintext_and_persists_hash() {
    const CONFIG: fn(&str) -> String = |secret| {
        format!(
            r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let item = mock.test.resource {{
  name    = "updated"
  comment = secret("{secret}")
}}
"#,
        )
    };

    let scenario = Scenario::new(&CONFIG("OLD_PLAINTEXT_3861"));
    assert_success(
        "initial live apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let initial_state = scenario.state_json();
    let old_state_hash = state_comment_hash(&initial_state, "updated").to_string();

    scenario.write_main(&CONFIG("NEW_PLAINTEXT_3861"));
    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(&scenario, &["OLD_PLAINTEXT_3861", "NEW_PLAINTEXT_3861"]);
    let plan = read_json(&scenario.project.join("plan.json"));
    assert!(
        contains_redacted_secret_hash(&plan, &old_state_hash),
        "the historical side of an update must retain the persisted digest",
    );
    let desired_plan_hash = plan_comment_placeholder_hash(&plan, "updated").to_string();
    assert_success("carina apply saved plan", &scenario.apply_plan());

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "updated")["comment"],
        "NEW_PLAINTEXT_3861",
    );
    let state = scenario.state_json();
    let saved = &state_resource_by_name(&state, "updated")["attributes"]["comment"];
    assert_secret_hash(saved);
    assert_ne!(saved, "NEW_PLAINTEXT_3861");
    assert_eq!(
        desired_plan_hash,
        state_comment_hash(&state, "updated"),
        "the desired update placeholder must match the state digest",
    );
}

#[test]
fn saved_plan_replace_sends_plaintext_and_persists_hash() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name    = "replace-old"
  comment = secret("REPLACE_OLD_PLAINTEXT_3861")
}
"#,
    );
    assert_success(
        "initial live apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let initial_state = scenario.state_json();
    let old_state_hash = state_comment_hash(&initial_state, "replace-old").to_string();
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name    = "replace-new"
  comment = secret("REPLACE_NEW_PLAINTEXT_3861")
}
"#,
    );

    assert_success("carina plan --out", &scenario.plan());
    assert_plan_is_redacted(
        &scenario,
        &["REPLACE_OLD_PLAINTEXT_3861", "REPLACE_NEW_PLAINTEXT_3861"],
    );
    let plan = read_json(&scenario.project.join("plan.json"));
    assert!(
        contains_redacted_secret_hash(&plan, &old_state_hash),
        "the historical side of a replacement must retain the persisted digest",
    );
    let desired_plan_hash = plan_comment_placeholder_hash(&plan, "replace-new").to_string();
    assert_success("carina apply saved plan", &scenario.apply_plan());

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "replace-new")["comment"],
        "REPLACE_NEW_PLAINTEXT_3861",
    );
    let state = scenario.state_json();
    assert_secret_hash(&state_resource_by_name(&state, "replace-new")["attributes"]["comment"]);
    assert_eq!(
        desired_plan_hash,
        state_comment_hash(&state, "replace-new"),
        "the desired replacement placeholder must match the state digest",
    );
}

#[test]
fn saved_plan_cbd_secret_replacement_keeps_temporary_name_and_applies() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name            = "cbd-secret"
  replacement_key = "before"
  comment         = secret("CBD_SECRET_PLAIN_3863")
  directives {
    create_before_destroy = true
  }
}
"#,
    );
    assert_success(
        "initial CBD fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name            = "cbd-secret"
  replacement_key = "after"
  comment         = secret("CBD_SECRET_PLAIN_3863")
  directives {
    create_before_destroy = true
  }
}
"#,
    );

    assert_success("CBD saved plan", &scenario.plan());
    let plan = read_json(&scenario.project.join("plan.json"));
    let create = plan["plan"]["effects"]
        .as_array()
        .expect("saved plan effects")
        .iter()
        .find_map(|effect| effect.get("Create"))
        .expect("CBD replacement create effect");
    let create_name = create
        .pointer("/attributes/name/String")
        .and_then(Value::as_str)
        .expect("CBD create name");
    assert!(
        create_name.starts_with("cbd-secret-") && create_name != "cbd-secret",
        "saved Create effect lost its temporary name: {create_name}"
    );
    assert_eq!(
        plan_resource_by_name(&plan, "cbd-secret")
            .pointer("/attributes/name/String")
            .and_then(Value::as_str),
        Some("cbd-secret"),
        "the planned resource remains the permanent desired value",
    );

    let apply = scenario.apply_plan();
    assert_success("CBD saved-plan apply", &apply);
    assert_output_has_no_plaintext("CBD saved-plan apply", &apply, &["CBD_SECRET_PLAIN_3863"]);
    let state = scenario.state_json();
    let row = state["resources"]
        .as_array()
        .and_then(|resources| resources.first())
        .expect("CBD state row");
    assert_eq!(row["identity"], "item");
    assert_secret_hash(&row["current"]["attributes"]["comment"]);
}

#[test]
fn saved_plan_cascade_keeps_secret_resource_ref_and_applies() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let producer = mock.test.resource {
  name            = "cascade-producer"
  replacement_key = "before"
  comment         = secret("CASCADE_SECRET_PLAIN_3863")
}

let consumer = mock.test.resource {
  name    = "cascade-consumer"
  comment = producer.comment
}
"#,
    );
    let initial_apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("initial cascade fixture apply", &initial_apply);
    assert_output_has_no_plaintext(
        "initial cascade fixture apply",
        &initial_apply,
        &["CASCADE_SECRET_PLAIN_3863"],
    );
    let initial_state = scenario.state_json();
    assert_secret_hash(
        &state_resource_by_name(&initial_state, "cascade-producer")["attributes"]["comment"],
    );
    assert_secret_hash(
        &state_resource_by_name(&initial_state, "cascade-consumer")["attributes"]["comment"],
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let producer = mock.test.resource {
  name            = "cascade-producer"
  replacement_key = "after"
  comment         = secret("CASCADE_SECRET_PLAIN_3863")
}

let consumer = mock.test.resource {
  name    = "cascade-consumer"
  comment = producer.comment
}
"#,
    );

    let saved_plan = scenario.plan();
    assert_success("cascade saved plan", &saved_plan);
    assert_output_has_no_plaintext(
        "cascade saved plan",
        &saved_plan,
        &["CASCADE_SECRET_PLAIN_3863"],
    );
    let plan = read_json(&scenario.project.join("plan.json"));
    let consumer_update = plan["plan"]["effects"]
        .as_array()
        .expect("saved plan effects")
        .iter()
        .filter_map(|effect| effect.get("Update"))
        .find(|update| {
            update.pointer("/to/attributes/name/String")
                == Some(&Value::String("cascade-consumer".to_string()))
        })
        .unwrap_or_else(|| panic!("saved plan has no cascade consumer update: {plan:#}"));
    assert!(
        consumer_update
            .pointer("/to/attributes/comment/ResourceRef")
            .is_some(),
        "cascade consumer effect lost its ResourceRef: {consumer_update:#}",
    );

    let apply = scenario.apply_plan();
    assert_success("cascade saved-plan apply", &apply);
    assert_output_has_no_plaintext(
        "cascade saved-plan apply",
        &apply,
        &["CASCADE_SECRET_PLAIN_3863"],
    );
    let state = scenario.state_json();
    assert_secret_hash(
        &state_resource_by_name(&state, "cascade-consumer")["attributes"]["comment"],
    );
}

#[test]
fn v16_saved_plan_requires_replan() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

mock.test.resource {
  name = "version-check"
}
"#,
    );
    assert_success("carina plan --out", &scenario.plan());
    let plan_path = scenario.project.join("plan.json");
    let mut plan = read_json(&plan_path);
    plan["version"] = Value::from(16);
    fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).expect("serialize legacy plan"),
    )
    .expect("write legacy plan");

    let apply = scenario.apply_plan();
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "v16 saved plan applied:\n{rendered}"
    );
    assert!(
        rendered.contains("Unsupported plan file version: 16") && rendered.contains("plan --out"),
        "v16 format must fail with replan guidance:\n{rendered}",
    );
}

#[test]
fn validate_and_plan_reject_secret_derived_backend_attribute() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(
        temp.path().join("main.crn"),
        r#"backend local {
  path = secret("carina.state.json")
}
provider mock {}
mock.test.resource { name = "backend-secret" }
"#,
    )
    .expect("write main.crn");

    for args in [vec!["validate", "."], vec!["plan", "."]] {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(temp.path())
            .env("NO_COLOR", "1")
            .args(&args)
            .output()
            .expect("run carina");
        let rendered = output_text(&output);
        assert!(
            !output.status.success(),
            "carina {} accepted a secret backend path:\n{rendered}",
            args[0],
        );
        assert!(
            rendered.contains("backend")
                && rendered.contains("path")
                && rendered.contains("line 2")
                && rendered.contains("secret")
                && rendered.contains("plain string"),
            "carina {} must explain that backend.path cannot be secret-derived:\n{rendered}",
            args[0],
        );
        assert!(
            !rendered.contains("carina.state.json"),
            "backend validation must not echo the secret-derived value:\n{rendered}",
        );
    }
}

#[test]
fn secret_free_saved_plan_ignores_later_unrelated_config_error() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}
mock.test.resource { name = "saved-without-secrets" }
"#,
    );
    let plan = scenario.plan();
    assert_success("carina plan --out", &plan);
    assert_eq!(
        count_redacted_secrets(&read_json(&scenario.project.join("plan.json"))),
        0,
        "fixture must exercise the placeholder-free path",
    );

    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}
mock.test.resource {
  name = "unrelated-invalid-current-config"
  attribute_that_does_not_exist = env("UNSET_AND_IRRELEVANT_TO_SAVED_PLAN_3861")
}
"#,
    );

    let apply = scenario.apply_plan();
    assert_success(
        "placeholder-free saved plan after unrelated config error",
        &apply,
    );
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "saved-without-secrets")["name"],
        "saved-without-secrets",
    );
}

#[test]
fn saved_plan_rehydrates_host_merged_default_tag_secret() {
    const ENV_NAME: &str = "SAVED_PLAN_DEFAULT_TAG_SECRET_3861";
    const PLAINTEXT: &str = "DEFAULT_TAG_PLAINTEXT_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {
  default_tags = {
    SecretTag = secret(env("SAVED_PLAN_DEFAULT_TAG_SECRET_3861"))
  }
}

mock.test.resource { name = "default-tag-secret" }
"#,
        &[(ENV_NAME, PLAINTEXT)],
    );

    let plan = scenario.plan_with_env(&[(ENV_NAME, PLAINTEXT)]);
    assert_success("carina plan --out", &plan);
    assert_output_has_no_plaintext("carina plan --out", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);
    let saved_plan = read_json(&scenario.project.join("plan.json"));
    assert!(
        saved_plan
            .pointer("/provider_configs/0/default_tags/SecretTag/RedactedSecret/hash")
            .and_then(Value::as_str)
            .is_some(),
        "the provider default-tag secret must retain its saved-plan placeholder: {saved_plan:#}",
    );
    assert!(
        plan_resource_by_name(&saved_plan, "default-tag-secret")
            .pointer("/attributes/tags/Map/SecretTag/RedactedSecret/hash")
            .and_then(Value::as_str)
            .is_some(),
        "the host-merged default tag must be redacted on the saved resource: {saved_plan:#}",
    );

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, PLAINTEXT)]);
    assert_success("carina apply saved plan", &apply);
    assert_output_has_no_plaintext("carina apply saved plan", &apply, &[PLAINTEXT]);

    let provider = scenario.provider_json();
    assert_eq!(
        provider_resource_by_name(&provider, "default-tag-secret")["tags"]["SecretTag"],
        PLAINTEXT,
        "saved-plan apply must rehydrate the host-merged default tag",
    );
    assert_secret_hash(
        &state_resource_by_name(&scenario.state_json(), "default-tag-secret")["attributes"]["tags"]
            ["SecretTag"],
    );
}

#[test]
fn saved_plan_rehydrates_anonymous_resource_after_state_identity_reconcile() {
    let config = |secret: &str| {
        format!(
            r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

mock.test.resource {{
  name    = "state-reconciled-anonymous"
  comment = secret("{secret}")
}}
"#,
        )
    };
    let scenario = Scenario::new(&config("ANONYMOUS_RECONCILE_OLD_3861"));
    assert_success(
        "initial live apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    let mut state = scenario.state_json();
    let old_identity = state["resources"][0]["identity"]
        .as_str()
        .expect("state identity")
        .to_string();
    let reconciled_identity = "state_preserved_anonymous_identity_3861";
    state["resources"][0]["identity"] = Value::String(reconciled_identity.to_string());
    state["resources"][0]["current"]["attributes"]["comment"] =
        PersistedValue::encode_provider_value(
            &CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
                ConcreteValue::String("ANONYMOUS_RECONCILE_OLD_3861".into()),
            )))),
            None,
        )
        .expect("rehash state secret for the reconciled identity")
        .as_json()
        .clone();
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).expect("serialize reconciled state"),
    )
    .expect("write reconciled state");

    let mut provider = scenario.provider_json();
    let provider_object = provider_resources_mut(&mut provider);
    let old_key = provider_object
        .keys()
        .next()
        .expect("provider resource key")
        .clone();
    let attributes = provider_object
        .remove(&old_key)
        .expect("provider resource attributes");
    let reconciled_key = old_key.replace(&old_identity, reconciled_identity);
    provider_object.insert(reconciled_key, attributes);
    fs::write(
        &scenario.mock_state,
        serde_json::to_vec_pretty(&provider).expect("serialize reconciled provider state"),
    )
    .expect("write reconciled provider state");

    scenario.write_main(&config("ANONYMOUS_RECONCILE_NEW_3861"));
    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);
    assert_plan_is_redacted(
        &scenario,
        &[
            "ANONYMOUS_RECONCILE_OLD_3861",
            "ANONYMOUS_RECONCILE_NEW_3861",
        ],
    );
    let apply = scenario.apply_plan();
    assert_success("carina apply saved plan", &apply);
    assert_output_has_no_plaintext(
        "carina apply saved plan",
        &apply,
        &[
            "ANONYMOUS_RECONCILE_OLD_3861",
            "ANONYMOUS_RECONCILE_NEW_3861",
        ],
    );
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "state-reconciled-anonymous")["comment"],
        "ANONYMOUS_RECONCILE_NEW_3861",
    );
}

#[test]
fn live_and_saved_plan_updates_never_print_historical_secret_plaintext() {
    let config = |secret: &str| {
        format!(
            r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}
let item = mock.test.resource {{
  name    = "masked-update-output"
  comment = secret("{secret}")
}}
"#,
        )
    };

    let live = Scenario::new(&config("LIVE_OUTPUT_OLD_SECRET_3861"));
    assert_success(
        "initial live apply",
        &live.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    live.write_main(&config("LIVE_OUTPUT_NEW_SECRET_3861"));
    let live_update = live.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("live update", &live_update);
    assert_output_has_no_plaintext(
        "live update",
        &live_update,
        &["LIVE_OUTPUT_OLD_SECRET_3861", "LIVE_OUTPUT_NEW_SECRET_3861"],
    );

    let saved = Scenario::new(&config("SAVED_OUTPUT_OLD_SECRET_3861"));
    assert_success(
        "initial saved-plan fixture apply",
        &saved.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    saved.write_main(&config("SAVED_OUTPUT_NEW_SECRET_3861"));
    let plan = saved.plan();
    assert_success("saved update plan", &plan);
    assert_output_has_no_plaintext(
        "saved update plan",
        &plan,
        &[
            "SAVED_OUTPUT_OLD_SECRET_3861",
            "SAVED_OUTPUT_NEW_SECRET_3861",
        ],
    );
    let saved_update = saved.apply_plan();
    assert_success("saved update apply", &saved_update);
    assert_output_has_no_plaintext(
        "saved update apply",
        &saved_update,
        &[
            "SAVED_OUTPUT_OLD_SECRET_3861",
            "SAVED_OUTPUT_NEW_SECRET_3861",
        ],
    );
}

#[test]
fn old_secret_is_masked_when_update_replaces_it_with_public_text() {
    const PLAINTEXT: &str = "PUBLIC_REPLACEMENT_OLD_SECRET_PLAINABC_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}
let item = mock.test.resource {
  name    = "secret-to-public-output"
  comment = "pre-${secret("PUBLIC_REPLACEMENT_OLD_SECRET_PLAINABC_3861")}-post"
}
"#,
    );
    assert_success(
        "initial secret-to-public fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}
let item = mock.test.resource {
  name    = "secret-to-public-output"
  comment = "public"
}
"#,
    );

    let live_plan = scenario.carina(&["plan", "."]);
    assert_success("live secret-to-public plan", &live_plan);
    assert_output_has_no_plaintext("live secret-to-public plan", &live_plan, &[PLAINTEXT]);

    let saved_plan = scenario.plan();
    assert_success("saved secret-to-public plan", &saved_plan);
    assert_output_has_no_plaintext("saved secret-to-public plan", &saved_plan, &[PLAINTEXT]);
    assert!(
        !scenario.plan_text().contains(PLAINTEXT),
        "saved plan leaked the historical secret plaintext",
    );

    let apply = scenario.apply_plan();
    assert_success("saved secret-to-public apply", &apply);
    assert_output_has_no_plaintext("saved secret-to-public apply", &apply, &[PLAINTEXT]);
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "secret-to-public-output")["comment"],
        "public",
    );
}

#[test]
fn old_secret_is_masked_when_replacement_uses_public_text() {
    const PLAINTEXT: &str = "PUBLIC_REPLACE_OLD_SECRET_PLAINABC_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}
let item = mock.test.resource {
  name    = "secret-replace-old"
  comment = "pre-${secret("PUBLIC_REPLACE_OLD_SECRET_PLAINABC_3861")}-post"
}
"#,
    );
    assert_success(
        "initial replacement fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}
let item = mock.test.resource {
  name    = "secret-replace-new"
  comment = "public"
}
"#,
    );

    let live_plan = scenario.carina(&["plan", "."]);
    assert_success("live secret replacement plan", &live_plan);
    assert_output_has_no_plaintext("live secret replacement plan", &live_plan, &[PLAINTEXT]);

    let saved_plan = scenario.plan();
    assert_success("saved secret replacement plan", &saved_plan);
    assert_output_has_no_plaintext("saved secret replacement plan", &saved_plan, &[PLAINTEXT]);
    assert!(!scenario.plan_text().contains(PLAINTEXT));

    let apply = scenario.apply_plan();
    assert_success("saved secret replacement apply", &apply);
    assert_output_has_no_plaintext("saved secret replacement apply", &apply, &[PLAINTEXT]);
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "secret-replace-new")["comment"],
        "public",
    );
}

#[test]
fn saved_plan_rehydrates_folded_secret_import_identifier() {
    const ENV_NAME: &str = "SAVED_PLAN_IMPORT_SECRET_3861";
    const PLAINTEXT: &str = "IMPORT_IDENTIFIER_PLAINTEXT_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let import_secret = secret(env("SAVED_PLAN_IMPORT_SECRET_3861"))

let imported = mock.test.resource {
  name = "imported-secret"
}

import {
  to = mock.test.resource "imported-secret"
  id = "${import_secret}|tail"
}
"#,
        &[(ENV_NAME, PLAINTEXT)],
    );

    let plan = scenario.plan_with_env(&[(ENV_NAME, PLAINTEXT)]);
    assert_success("carina plan --out", &plan);
    assert_output_has_no_plaintext("carina plan --out", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);

    fs::write(
        &scenario.mock_state,
        serde_json::to_vec_pretty(&serde_json::json!({
            "test.resource.imported-secret": {
                "name": "imported-secret",
                "comment": "provider-existing"
            }
        }))
        .expect("serialize import provider state"),
    )
    .expect("write import provider state");

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, PLAINTEXT)]);
    assert_success("carina apply saved import plan", &apply);
    assert_output_has_no_plaintext("carina apply saved import plan", &apply, &[PLAINTEXT]);
    assert_eq!(
        state_resource_by_name(&scenario.state_json(), "imported-secret")["attributes"]["name"],
        "imported-secret",
    );
}

#[test]
fn saved_plan_reports_changed_historical_update_secret_as_drift() {
    let config = |secret: &str| {
        format!(
            r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let item = mock.test.resource {{
  name    = "historical-update-secret"
  comment = secret("{secret}")
}}
"#,
        )
    };
    let scenario = Scenario::new(&config("HISTORICAL_UPDATE_OLD_3861"));
    assert_success(
        "initial live apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(&config("HISTORICAL_UPDATE_NEW_3861"));
    let plan = scenario.plan();
    assert_success("saved update plan", &plan);
    assert_output_has_no_plaintext(
        "saved update plan",
        &plan,
        &["HISTORICAL_UPDATE_OLD_3861", "HISTORICAL_UPDATE_NEW_3861"],
    );
    assert_plan_is_redacted(
        &scenario,
        &["HISTORICAL_UPDATE_OLD_3861", "HISTORICAL_UPDATE_NEW_3861"],
    );

    let mut provider = scenario.provider_json();
    provider_resource_by_name_mut(&mut provider, "historical-update-secret")["comment"] =
        Value::String("HISTORICAL_UPDATE_DRIFTED_3861".to_string());
    fs::write(
        &scenario.mock_state,
        serde_json::to_vec_pretty(&provider).expect("serialize drifted provider state"),
    )
    .expect("write drifted provider state");
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan();
    let rendered = output_text(&apply);
    assert!(!apply.status.success(), "drifted plan applied:\n{rendered}");
    assert!(
        rendered.contains("Infrastructure drift detected")
            && rendered.contains("attributes have changed since plan was created"),
        "historical secret mutation must be infrastructure drift:\n{rendered}",
    );
    assert!(
        !rendered.contains("secret changed since the plan"),
        "provider-side drift must not be reported as a configuration change:\n{rendered}",
    );
    assert_output_has_no_plaintext(
        "historical update drift",
        &apply,
        &[
            "HISTORICAL_UPDATE_OLD_3861",
            "HISTORICAL_UPDATE_NEW_3861",
            "HISTORICAL_UPDATE_DRIFTED_3861",
        ],
    );
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state changed after drift rejection",
    );
    assert_eq!(
        fs::read(&scenario.mock_state).ok(),
        provider_before,
        "provider state changed after drift rejection",
    );
}

#[test]
fn saved_plan_reports_vanished_secret_resource_as_drift_before_verification() {
    const OLD: &str = "VANISHED_RESOURCE_OLD_SECRET_3861";
    const NEW: &str = "VANISHED_RESOURCE_NEW_SECRET_3861";
    let config = |secret: &str| {
        format!(
            r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let item = mock.test.resource {{
  name    = "vanished-secret-resource"
  comment = secret("{secret}")
}}
"#,
        )
    };
    let scenario = Scenario::new(&config(OLD));
    assert_success(
        "initial vanished-resource fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(&config(NEW));
    let plan = scenario.plan();
    assert_success("vanished-resource saved plan", &plan);
    assert_output_has_no_plaintext("vanished-resource saved plan", &plan, &[OLD, NEW]);
    assert_plan_is_redacted(&scenario, &[OLD, NEW]);

    fs::write(&scenario.mock_state, b"{}\n").expect("clear provider state out of band");
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan();
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "saved plan unexpectedly applied after the resource vanished:\n{rendered}",
    );
    assert!(
        rendered.contains("Infrastructure drift detected")
            && rendered.contains("resource existed at plan time but no longer exists"),
        "a vanished resource must use the ordinary drift outcome before secret verification:\n{rendered}",
    );
    assert!(
        !rendered.contains("recorded secret position no longer exists"),
        "secret verification must not replace the vanished-resource drift outcome:\n{rendered}",
    );
    assert_output_has_no_plaintext("vanished-resource saved apply", &apply, &[OLD, NEW]);
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state changed after drift rejection",
    );
    assert_eq!(
        fs::read(&scenario.mock_state).ok(),
        provider_before,
        "provider state changed after drift rejection",
    );
}

#[test]
fn saved_plan_moved_secret_resource_reads_and_verifies_the_origin() {
    const PLAINTEXT: &str = "MOVED_RESOURCE_SECRET_PLAINABC_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let r = mock.test.resource {
  name    = "moved-secret-resource"
  comment = secret("MOVED_RESOURCE_SECRET_PLAINABC_3861")
}
"#,
    );
    assert_success(
        "initial moved-resource fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

moved {
  from = mock.test.resource 'r'
  to   = mock.test.resource 'r2'
}

let r2 = mock.test.resource {
  name    = "moved-secret-resource"
  comment = secret("MOVED_RESOURCE_SECRET_PLAINABC_3861")
}
"#,
    );

    let plan = scenario.plan();
    assert_success("moved-resource saved plan", &plan);
    assert_output_has_no_plaintext("moved-resource saved plan", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);
    let apply = scenario.apply_plan();
    assert_success("moved-resource saved apply", &apply);
    assert_output_has_no_plaintext("moved-resource saved apply", &apply, &[PLAINTEXT]);
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "moved-secret-resource")["comment"],
        PLAINTEXT,
    );
    assert_secret_hash(
        &state_resource_by_name(&scenario.state_json(), "moved-secret-resource")["attributes"]["comment"],
    );
}

#[test]
fn saved_plan_rejects_changed_anonymous_resource_secret() {
    const ENV_NAME: &str = "SAVED_PLAN_ANONYMOUS_CHANGED_3861";
    const PLANNED: &str = "ANONYMOUS_PLANNED_SECRET_3861";
    const CHANGED: &str = "ANONYMOUS_CHANGED_SECRET_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

mock.test.resource {
  name    = "anonymous-secret-change"
  comment = secret(env("SAVED_PLAN_ANONYMOUS_CHANGED_3861"))
}
"#,
        &[(ENV_NAME, PLANNED)],
    );
    let plan = scenario.plan_with_env(&[(ENV_NAME, PLANNED)]);
    assert_success("anonymous secret plan", &plan);
    assert_output_has_no_plaintext("anonymous secret plan", &plan, &[PLANNED]);
    assert_plan_is_redacted(&scenario, &[PLANNED]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, CHANGED)]);
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[PLANNED, CHANGED],
        "secret changed since the plan",
        state_before,
        provider_before,
    );
}

#[test]
fn saved_plan_rejects_changed_provider_default_tag_secret() {
    const ENV_NAME: &str = "SAVED_PLAN_DEFAULT_TAG_CHANGED_3861";
    const PLANNED: &str = "DEFAULT_TAG_PLANNED_SECRET_3861";
    const CHANGED: &str = "DEFAULT_TAG_CHANGED_SECRET_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {
  default_tags = {
    SecretTag = secret(env("SAVED_PLAN_DEFAULT_TAG_CHANGED_3861"))
  }
}

mock.test.resource { name = "default-tag-change" }
"#,
        &[(ENV_NAME, PLANNED)],
    );
    let plan = scenario.plan_with_env(&[(ENV_NAME, PLANNED)]);
    assert_success("default-tags secret plan", &plan);
    assert_output_has_no_plaintext("default-tags secret plan", &plan, &[PLANNED]);
    assert_plan_is_redacted(&scenario, &[PLANNED]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, CHANGED)]);
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[PLANNED, CHANGED],
        "secret changed since the plan",
        state_before,
        provider_before,
    );
}

#[test]
fn saved_plan_rejects_changed_data_source_secret() {
    const ENV_NAME: &str = "SAVED_PLAN_DATA_SOURCE_CHANGED_3861";
    const PLANNED: &str = "DATA_SOURCE_PLANNED_SECRET_3861";
    const CHANGED: &str = "DATA_SOURCE_CHANGED_SECRET_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let roles = read mock.iam.Roles {
  name_regex = secret(env("SAVED_PLAN_DATA_SOURCE_CHANGED_3861"))
}
"#,
        &[(ENV_NAME, PLANNED)],
    );
    let plan = scenario.plan_with_env(&[(ENV_NAME, PLANNED)]);
    assert_success("data-source secret plan", &plan);
    assert_output_has_no_plaintext("data-source secret plan", &plan, &[PLANNED]);
    assert_plan_is_redacted(&scenario, &[PLANNED]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, CHANGED)]);
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[PLANNED, CHANGED],
        "secret changed since the plan",
        state_before,
        provider_before,
    );
}

#[test]
fn saved_plan_rejects_changed_folded_import_secret() {
    const ENV_NAME: &str = "SAVED_PLAN_IMPORT_CHANGED_3861";
    const PLANNED: &str = "IMPORT_PLANNED_SECRET_3861";
    const CHANGED: &str = "IMPORT_CHANGED_SECRET_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let import_secret = secret(env("SAVED_PLAN_IMPORT_CHANGED_3861"))
let imported = mock.test.resource { name = "changed-import-secret" }

import {
  to = mock.test.resource "changed-import-secret"
  id = "${import_secret}|tail"
}
"#,
        &[(ENV_NAME, PLANNED)],
    );
    let plan = scenario.plan_with_env(&[(ENV_NAME, PLANNED)]);
    assert_success("import secret plan", &plan);
    assert_output_has_no_plaintext("import secret plan", &plan, &[PLANNED]);
    assert_plan_is_redacted(&scenario, &[PLANNED]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, CHANGED)]);
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[PLANNED, CHANGED],
        "secret changed since the plan",
        state_before,
        provider_before,
    );
}

#[test]
fn saved_plan_rejects_changed_deferred_for_template_secret() {
    const ENV_NAME: &str = "SAVED_PLAN_DEFERRED_FOR_CHANGED_3861";
    const PLANNED: &str = "DEFERRED_FOR_PLANNED_SECRET_3861";
    const CHANGED: &str = "DEFERRED_FOR_CHANGED_SECRET_3861";
    let scenario = Scenario::new_with_env(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let origin = mock.test.resource { name = "deferred-for-origin" }
let roles = read mock.iam.Roles {
  name_regex = origin.identifier
}

for role_name in roles.names {
  mock.test.resource {
    name    = role_name
    comment = secret(env("SAVED_PLAN_DEFERRED_FOR_CHANGED_3861"))
  }
}
"#,
        &[(ENV_NAME, PLANNED)],
    );
    let plan = scenario.plan_with_env(&[(ENV_NAME, PLANNED)]);
    assert_success("deferred-for secret plan", &plan);
    assert_output_has_no_plaintext("deferred-for secret plan", &plan, &[PLANNED]);
    assert_plan_is_redacted(&scenario, &[PLANNED]);
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan_with_env(&[(ENV_NAME, CHANGED)]);
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[PLANNED, CHANGED],
        "secret changed since the plan",
        state_before,
        provider_before,
    );
}

#[test]
fn saved_plan_rejects_changed_orphan_delete_secret() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let orphan = mock.test.resource {
  name    = "orphan-secret-delete"
  comment = secret("ORPHAN_DELETE_PLANNED_SECRET_3861")
}
"#,
    );
    assert_success(
        "initial orphan fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
    );
    let plan = scenario.plan();
    assert_success("orphan delete secret plan", &plan);
    assert_output_has_no_plaintext(
        "orphan delete secret plan",
        &plan,
        &["ORPHAN_DELETE_PLANNED_SECRET_3861"],
    );
    assert_plan_is_redacted(&scenario, &["ORPHAN_DELETE_PLANNED_SECRET_3861"]);

    let mut provider = scenario.provider_json();
    provider_resource_by_name_mut(&mut provider, "orphan-secret-delete")["comment"] =
        Value::String("ORPHAN_DELETE_DRIFTED_SECRET_3861".to_string());
    fs::write(
        &scenario.mock_state,
        serde_json::to_vec_pretty(&provider).expect("serialize drifted orphan provider state"),
    )
    .expect("write drifted orphan provider state");
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan();
    assert_secret_verification_rejected_without_mutation(
        &scenario,
        &apply,
        &[
            "ORPHAN_DELETE_PLANNED_SECRET_3861",
            "ORPHAN_DELETE_DRIFTED_SECRET_3861",
        ],
        "secret changed since the plan",
        state_before,
        provider_before,
    );
}

#[test]
fn refresh_false_saved_plan_reuses_moved_state_secret_digest() {
    const PLAINTEXT: &str = "MOVE_PLAIN_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let old = mock.test.resource {
  name    = "moved-secret-digest"
  comment = secret("MOVE_PLAIN_3861")
}
"#,
    );
    let initial_apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("initial moved-secret apply", &initial_apply);
    assert_output_has_no_plaintext("initial moved-secret apply", &initial_apply, &[PLAINTEXT]);
    let old_hash = state_comment_hash(&scenario.state_json(), "moved-secret-digest").to_string();

    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

moved {
  from = mock.test.resource 'old'
  to   = mock.test.resource 'renamed'
}

let renamed = mock.test.resource {
  name    = "moved-secret-digest"
  comment = secret("MOVE_PLAIN_3861")
}
"#,
    );
    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
    assert_success("refresh=false moved-secret plan", &plan);
    assert!(
        output_text(&plan).contains("Plan: 0 to add, 0 to change, 0 to destroy"),
        "an unchanged moved secret must remain a pure move:\n{}",
        output_text(&plan),
    );
    assert_output_has_no_plaintext("refresh=false moved-secret plan", &plan, &[PLAINTEXT]);
    assert_plan_is_redacted(&scenario, &[PLAINTEXT]);
    let saved_plan = read_json(&scenario.project.join("plan.json"));
    assert_eq!(
        current_state_comment_placeholder_hash(&saved_plan, "renamed"),
        old_hash,
        "a persisted state hash must be lifted, never hashed as plaintext under the moved-to identity",
    );

    let apply = scenario.apply_plan();
    assert_success("apply refresh=false moved-secret plan", &apply);
    assert_output_has_no_plaintext(
        "apply refresh=false moved-secret plan",
        &apply,
        &[PLAINTEXT],
    );
    assert_eq!(
        provider_resource_by_name(&scenario.provider_json(), "moved-secret-digest")["comment"],
        PLAINTEXT,
    );
    let state = scenario.state_json();
    assert_secret_hash(
        &state_resource_by_name(&state, "moved-secret-digest")["attributes"]["comment"],
    );
    assert_eq!(
        state_comment_hash(&state, "moved-secret-digest"),
        old_hash,
        "saved-plan apply must write the exact digest selected from the pre-move state",
    );
}

#[test]
fn remote_secret_change_is_reported_as_infrastructure_drift() {
    const PLANNED: &str = "REMOTE_DRIFT_PLANNED_SECRET_3861";
    const DRIFTED: &str = "REMOTE_DRIFT_ACTUAL_SECRET_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name    = "remote-secret-drift"
  comment = secret("REMOTE_DRIFT_PLANNED_SECRET_3861")
}
"#,
    );
    let initial_apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("initial remote-secret fixture apply", &initial_apply);
    assert_output_has_no_plaintext(
        "initial remote-secret fixture apply",
        &initial_apply,
        &[PLANNED],
    );

    let plan = scenario.plan();
    assert_success("remote-secret saved plan", &plan);
    assert_output_has_no_plaintext("remote-secret saved plan", &plan, &[PLANNED]);
    assert_plan_is_redacted(&scenario, &[PLANNED]);

    let mut provider = scenario.provider_json();
    provider_resource_by_name_mut(&mut provider, "remote-secret-drift")["comment"] =
        Value::String(DRIFTED.to_string());
    fs::write(
        &scenario.mock_state,
        serde_json::to_vec_pretty(&provider).expect("serialize drifted provider state"),
    )
    .expect("write drifted provider state");
    let state_before = fs::read(scenario.project.join("carina.state.json")).ok();
    let provider_before = fs::read(&scenario.mock_state).ok();

    let apply = scenario.apply_plan();
    let rendered = output_text(&apply);
    assert!(
        !apply.status.success(),
        "drifted saved plan applied:\n{rendered}"
    );
    assert!(
        rendered.contains("Infrastructure drift detected")
            && rendered.contains("attributes have changed since plan was created"),
        "remote secret drift must use the ordinary infrastructure-drift outcome:\n{rendered}",
    );
    assert!(
        !rendered.contains("secret changed since the plan"),
        "remote drift must not be classified as a configuration-secret change:\n{rendered}",
    );
    assert_output_has_no_plaintext("remote-secret saved apply", &apply, &[PLANNED, DRIFTED]);
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).ok(),
        state_before,
        "state changed after drift rejection",
    );
    assert_eq!(
        fs::read(&scenario.mock_state).ok(),
        provider_before,
        "provider state changed after drift rejection",
    );
}

#[test]
fn plan_rejects_export_of_state_retained_secret_before_apply() {
    const PLAINTEXT: &str = "EXPORT_STATE_SECRET_HUNTER2_3861";
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name    = "state-secret-export"
  comment = secret("EXPORT_STATE_SECRET_HUNTER2_3861")
}
"#,
    );
    let initial_apply = scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]);
    assert_success("initial export-secret fixture apply", &initial_apply);
    assert_output_has_no_plaintext(
        "initial export-secret fixture apply",
        &initial_apply,
        &[PLAINTEXT],
    );

    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let item = mock.test.resource {
  name = "state-secret-export"
}

exports {
  retained: String = item.comment
}
"#,
    );

    let provider_before = fs::read(&scenario.mock_state).expect("provider fixture state");
    let state_before = fs::read(scenario.project.join("carina.state.json")).expect("Carina state");
    let plan = scenario.carina(&["plan", "."]);
    let rendered = output_text(&plan);
    assert!(
        !plan.status.success(),
        "secret export unexpectedly planned:\n{rendered}"
    );
    assert!(
        rendered.contains("export 'retained' cannot hold a secret value"),
        "plan used the wrong secret-export diagnostic:\n{rendered}",
    );
    assert_output_has_no_plaintext("secret export plan rejection", &plan, &[PLAINTEXT]);
    assert_eq!(
        fs::read(&scenario.mock_state).expect("provider fixture state after plan"),
        provider_before,
        "plan rejection mutated provider state",
    );
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).expect("Carina state after plan"),
        state_before,
        "plan rejection mutated Carina state",
    );
}
