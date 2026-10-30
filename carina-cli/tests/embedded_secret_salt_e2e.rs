use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use argon2::Argon2;
use serde_json::Value;
use tempfile::TempDir;

use carina_core::resource::RedactedSecretHash;
use carina_core::value::PERSISTED_SECRET_KEY;

const V2_SECRET_PREFIX: &str = "$argon2id$";
const SELF_CONTAINED_LEGACY_PREFIX: &str = "argon2-legacy:";

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new(main: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        fs::write(project.join("main.crn"), main).expect("write main.crn");
        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn write_main(&self, main: &str) {
        fs::write(self.project.join("main.crn"), main).expect("rewrite main.crn");
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn state_json(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.project.join("carina.state.json")).expect("read state"),
        )
        .expect("parse state")
    }

    fn write_state_json(&self, state: &Value) {
        fs::write(
            self.project.join("carina.state.json"),
            serde_json::to_vec_pretty(state).expect("encode state"),
        )
        .expect("write state");
    }

    fn rename_mock_resource_identity(&self, resource_type: &str, from: &str, to: &str) {
        let mut state: Value =
            serde_json::from_slice(&fs::read(&self.mock_state).expect("read mock provider state"))
                .expect("parse mock provider state");
        let states = state["states"]
            .as_object_mut()
            .expect("mock provider states object");
        let from = format!("{resource_type}.{from}");
        let to = format!("{resource_type}.{to}");
        let resource = states
            .remove(&from)
            .unwrap_or_else(|| panic!("missing mock provider resource {from:?}"));
        assert!(
            states.insert(to.clone(), resource).is_none(),
            "mock provider resource {to:?} already exists"
        );
        fs::write(
            &self.mock_state,
            serde_json::to_vec_pretty(&state).expect("encode mock provider state"),
        )
        .expect("write mock provider state");
    }
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
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_plan_counts(output: &Output, add: usize, change: usize, destroy: usize) {
    assert_success("carina plan", output);
    let rendered = output_text(output);
    if add == 0 && change == 0 && destroy == 0 && rendered.contains("No changes.") {
        return;
    }
    let expected = format!("Plan: {add} to add, {change} to change, {destroy} to destroy");
    assert!(
        rendered.contains(&expected),
        "expected {expected:?} in plan output:\n{rendered}",
    );
}

fn resource_by_identity<'a>(state: &'a Value, identity: &str) -> &'a Value {
    let resource = state["resources"]
        .as_array()
        .expect("state resources")
        .iter()
        .find(|resource| resource["identity"] == identity)
        .unwrap_or_else(|| panic!("missing state resource {identity:?}: {state:#}"));
    &resource["current"]
}

fn downgrade_state_to_v13(state: &mut Value) {
    state["version"] = Value::from(13);
    state
        .as_object_mut()
        .expect("state object")
        .remove("exports_version");
    for row in state["resources"].as_array_mut().expect("state resources") {
        let row_object = row.as_object_mut().expect("state resource row");
        let current = row_object
            .remove("current")
            .expect("v15 row has a current instance");
        let mut current = current
            .as_object()
            .expect("current instance object")
            .clone();
        assert_eq!(
            current.remove("kind"),
            Some(Value::String("applied".to_string())),
            "legacy-secret fixtures start from applied rows"
        );
        row_object.insert(
            "identifier".to_string(),
            current.remove("identifier").expect("applied identifier"),
        );
        row_object.insert(
            "attributes".to_string(),
            current.remove("attributes").expect("applied attributes"),
        );
        if let Some(value) = current.remove("write_only_attributes") {
            row_object.insert("write_only_attributes".to_string(), value);
        }
        if let Some(value) = current.remove("partial_read") {
            row_object.insert("partial_read".to_string(), value);
        }
        assert_eq!(
            current.remove("nested_write_only"),
            Some(serde_json::json!({})),
            "legacy-secret fixtures must not carry nested write-only provenance"
        );
        assert!(
            current.is_empty(),
            "unexpected v15 current fields: {current:?}"
        );
    }
}

fn comment_digest(state: &Value, identity: &str) -> String {
    secret_digest(&resource_by_identity(state, identity)["attributes"]["comment"]).to_string()
}

fn secret_digest(value: &Value) -> &str {
    let object = value.as_object().expect("secret tag");
    assert_eq!(object.len(), 1, "secret tag must contain exactly one entry");
    object
        .get(PERSISTED_SECRET_KEY)
        .and_then(Value::as_str)
        .expect("secret tag must carry a digest")
}

fn assert_digest_verifies(encoded_digest: &str, plaintext: &str) {
    let digest = RedactedSecretHash::from_encoded_digest(encoded_digest)
        .unwrap_or_else(|| panic!("invalid persisted secret digest: {encoded_digest}"));
    let material = serde_json::to_vec(&Value::String(plaintext.to_string()))
        .expect("serialize secret material");
    assert!(
        digest.verify(material),
        "digest did not verify its plaintext"
    );
}

fn legacy_marker(
    plaintext: &Value,
    display_type: &str,
    identity: &str,
    top_level_attribute_key: &str,
) -> String {
    let material = serde_json::to_vec(plaintext).expect("serialize legacy secret material");
    let salt = format!("carina:{display_type}:{identity}:{top_level_attribute_key}");
    let mut output = [0_u8; 32];
    Argon2::default()
        .hash_password_into(&material, salt.as_bytes(), &mut output)
        .expect("hash pre-v12 legacy fixture");
    let hash = output
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("_secret:argon2:{hash}")
}

#[test]
fn equal_nested_secrets_get_distinct_map_and_list_digests() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let mapped = mock.test.resource {
  name = "nested-map-secrets"
  tags = {
    x = secret("NESTED_EQUAL_PLAIN")
    y = secret("NESTED_EQUAL_PLAIN")
  }
}

let listed = mock.example.consumer {
  list = [
    secret("NESTED_EQUAL_PLAIN"),
    secret("NESTED_EQUAL_PLAIN"),
  ]
}
"#,
    );

    assert_success(
        "apply nested equal secrets",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let state = scenario.state_json();
    let tags = &resource_by_identity(&state, "mapped")["attributes"]["tags"];
    let map_x = secret_digest(&tags["x"]);
    let map_y = secret_digest(&tags["y"]);
    assert_ne!(map_x, map_y, "equal map secrets leaked equality");
    assert_digest_verifies(map_x, "NESTED_EQUAL_PLAIN");
    assert_digest_verifies(map_y, "NESTED_EQUAL_PLAIN");

    let list = resource_by_identity(&state, "listed")["attributes"]["list"]
        .as_array()
        .expect("secret list");
    assert_ne!(list[0], list[1], "equal list secrets leaked equality");
    assert_digest_verifies(secret_digest(&list[0]), "NESTED_EQUAL_PLAIN");
    assert_digest_verifies(secret_digest(&list[1]), "NESTED_EQUAL_PLAIN");
}

#[test]
fn moved_secret_is_a_pure_move_with_and_without_refresh() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let old = mock.test.resource {
  name    = "moved-secret"
  comment = secret("MOVE_PLAIN")
}
"#,
    );
    assert_success(
        "initial moved-secret apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let before_move = comment_digest(&scenario.state_json(), "old");
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

moved {
  from = mock.test.resource "old"
  to   = mock.test.resource "renamed"
}

let renamed = mock.test.resource {
  name    = "moved-secret"
  comment = secret("MOVE_PLAIN")
}
"#,
    );

    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
    assert_plan_counts(&scenario.carina(&["plan", "."]), 0, 0, 0);
    assert_success(
        "apply pure moved-secret rename",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    assert_eq!(
        comment_digest(&scenario.state_json(), "renamed"),
        before_move,
        "a pure move must reuse the source row's byte-identical digest",
    );
}

#[test]
fn moved_legacy_secret_is_a_pure_move_and_saved_plan_applies() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let old = mock.test.resource {
  name    = "moved-legacy-secret"
  comment = secret("MOVED_LEGACY_PLAIN")
}
"#,
    );
    assert_success(
        "apply moved legacy fixture",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    state["resources"][0]["attributes"]["comment"] = Value::String(legacy_marker(
        &Value::String("MOVED_LEGACY_PLAIN".to_string()),
        "mock.test.resource",
        "old",
        "comment",
    ));
    scenario.write_state_json(&state);
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

moved {
  from = mock.test.resource "old"
  to   = mock.test.resource "renamed"
}

let renamed = mock.test.resource {
  name    = "moved-legacy-secret"
  comment = secret("MOVED_LEGACY_PLAIN")
}
"#,
    );

    assert_plan_counts(
        &scenario.carina(&["plan", "--refresh=false", "--out", "moved-legacy.json", "."]),
        0,
        0,
        0,
    );
    assert_success(
        "apply moved legacy saved plan",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "moved-legacy.json",
        ]),
    );
    assert_digest_verifies(
        &comment_digest(&scenario.state_json(), "renamed"),
        "MOVED_LEGACY_PLAIN",
    );
}

#[test]
fn nested_secret_wrappers_converge_and_saved_plan_applies() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let nested = mock.test.resource {
  name = "nested-secret-wrapper"
  tags = secret({
    a = secret("NESTED_WRAPPER_PLAIN")
  })
}
"#,
    );
    assert_success(
        "apply nested secret wrapper",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
    assert_plan_counts(
        &scenario.carina(&["plan", "--refresh=false", "--out", "nested-plan.json", "."]),
        0,
        0,
        0,
    );
    assert_success(
        "apply nested-secret saved plan",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "nested-plan.json",
        ]),
    );
}

#[test]
fn orphan_rekey_with_legacy_secret_applies_saved_plan() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let old = mock.test.resource {
  name    = "orphan-rekey-legacy"
  comment = secret("ORPHAN_REKEY_LEGACY_PLAIN")
}
"#,
    );
    assert_success(
        "apply orphan-rekey fixture",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    state["resources"][0]["attributes"]["comment"] = Value::String(legacy_marker(
        &Value::String("ORPHAN_REKEY_LEGACY_PLAIN".to_string()),
        "mock.test.resource",
        "old",
        "comment",
    ));
    scenario.write_state_json(&state);
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

moved {
  from = mock.test.resource "old"
  to   = mock.test.resource "renamed_orphan"
}
"#,
    );

    assert_plan_counts(
        &scenario.carina(&["plan", "--refresh=false", "--out", "orphan-rekey.json", "."]),
        0,
        0,
        0,
    );
    let plan = fs::read_to_string(scenario.project.join("orphan-rekey.json"))
        .expect("read orphan-rekey saved plan");
    assert!(
        plan.contains("argon2-legacy:"),
        "the saved plan must carry the orphan's self-contained legacy digest",
    );
    assert_success(
        "apply orphan-rekey saved plan",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "orphan-rekey.json",
        ]),
    );
    assert!(
        scenario.state_json()["resources"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "the undeclared orphan destination must be removed after apply",
    );
}

#[test]
fn moved_removed_legacy_secret_can_be_readded_without_an_update() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let old = mock.test.resource {
  name    = "round-two-legacy"
  comment = secret("ROUND_TWO_SAME_PLAIN")
}
"#,
    );
    assert_success(
        "apply round-two legacy fixture",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    state["resources"][0]["attributes"]["comment"] = Value::String(legacy_marker(
        &Value::String("ROUND_TWO_SAME_PLAIN".to_string()),
        "mock.test.resource",
        "old",
        "comment",
    ));
    scenario.write_state_json(&state);
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

moved {
  from = mock.test.resource "old"
  to   = mock.test.resource "renamed"
}

let renamed = mock.test.resource {
  name = "round-two-legacy"
}
"#,
    );
    assert_success(
        "plan move while removing legacy secret",
        &scenario.carina(&[
            "plan",
            "--refresh=false",
            "--out",
            "remove-secret.json",
            ".",
        ]),
    );
    assert_success(
        "apply move while removing legacy secret",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "remove-secret.json",
        ]),
    );
    let carried = comment_digest(&scenario.state_json(), "renamed");
    assert!(
        carried.starts_with(SELF_CONTAINED_LEGACY_PREFIX),
        "a migrated legacy digest must carry its original salt: {carried}",
    );

    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let renamed = mock.test.resource {
  name    = "round-two-legacy"
  comment = secret("ROUND_TWO_SAME_PLAIN")
}
"#,
    );
    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
}

#[test]
fn anonymous_identity_reconcile_preserves_self_contained_legacy_digest() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

mock.example.consumer {
  name  = "anonymous-legacy-reconcile"
  plain = secret("ANONYMOUS_LEGACY_RECONCILE_PLAIN")
}
"#,
    );
    assert_success(
        "apply anonymous legacy fixture",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    let old_identity = state["resources"][0]["identity"]
        .as_str()
        .expect("anonymous state identity")
        .to_string();
    let stored_identity = old_identity
        .strip_prefix("mock_")
        .expect("current anonymous identity includes the provider prefix")
        .to_string();
    state["resources"][0]["identity"] = Value::String(stored_identity.clone());
    scenario.rename_mock_resource_identity("example.consumer", &old_identity, &stored_identity);
    state["resources"][0]["attributes"]["plain"] = Value::String(legacy_marker(
        &Value::String("ANONYMOUS_LEGACY_RECONCILE_PLAIN".to_string()),
        "mock.example.consumer",
        &stored_identity,
        "plain",
    ));
    scenario.write_state_json(&state);

    assert_plan_counts(
        &scenario.carina(&[
            "plan",
            "--refresh=false",
            "--out",
            "anonymous-legacy.json",
            ".",
        ]),
        0,
        0,
        0,
    );
    assert_success(
        "apply anonymous legacy saved plan",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "anonymous-legacy.json",
        ]),
    );
}

#[test]
fn repeated_apply_reuses_the_exact_secret_digest() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let stable = mock.test.resource {
  name    = "stable-secret"
  comment = secret("STABLE_PLAIN")
}
"#,
    );
    assert_success(
        "first stable-secret apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let first = comment_digest(&scenario.state_json(), "stable");
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let stable = mock.test.resource {
  name        = "stable-secret"
  comment     = secret("STABLE_PLAIN")
  web_acl_arn = "force-second-writeback"
}
"#,
    );
    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 1, 0);
    assert_success(
        "second stable-secret apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let second = comment_digest(&scenario.state_json(), "stable");
    assert_eq!(first, second, "unchanged secret digest churned");
    assert_plan_counts(&scenario.carina(&["plan", "."]), 0, 0, 0);
}

#[test]
fn matching_legacy_markers_plan_no_change_then_write_back_as_v2() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy = mock.test.resource {
  name    = "legacy-secret"
  comment = secret("LEGACY_TOP_PLAIN")
  tags = {
    nested = secret("LEGACY_NESTED_PLAIN")
  }
}
"#,
    );
    assert_success(
        "initial legacy fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    let resource = state["resources"]
        .as_array_mut()
        .expect("state resources")
        .iter_mut()
        .find(|resource| resource["identity"] == "legacy")
        .expect("legacy state row");
    resource["attributes"]["comment"] = Value::String(legacy_marker(
        &Value::String("LEGACY_TOP_PLAIN".to_string()),
        "mock.test.resource",
        "legacy",
        "comment",
    ));
    resource["attributes"]["tags"]["nested"] = Value::String(legacy_marker(
        &Value::String("LEGACY_NESTED_PLAIN".to_string()),
        "mock.test.resource",
        "legacy",
        "tags",
    ));
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).expect("encode legacy state"),
    )
    .expect("write legacy state");

    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
    assert_plan_counts(&scenario.carina(&["plan", "."]), 0, 0, 0);
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy = mock.test.resource {
  name        = "legacy-secret"
  comment     = secret("LEGACY_TOP_PLAIN")
  web_acl_arn = "upgrade-writeback"
  tags = {
    nested = secret("LEGACY_NESTED_PLAIN")
  }
}
"#,
    );
    assert_success(
        "apply legacy marker upgrade",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let state = scenario.state_json();
    let digest = comment_digest(&state, "legacy");
    assert!(
        digest.starts_with(V2_SECRET_PREFIX),
        "legacy marker was not replaced with a v2 digest: {digest}",
    );
    let nested =
        secret_digest(&resource_by_identity(&state, "legacy")["attributes"]["tags"]["nested"]);
    assert!(
        nested.starts_with(V2_SECRET_PREFIX),
        "nested legacy marker was not replaced with v2: {nested}",
    );
    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
}

#[test]
fn saved_plan_from_matching_legacy_state_applies_and_upgrades_to_v2() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy_plan = mock.test.resource {
  name    = "legacy-saved-plan"
  comment = secret("LEGACY_SAVED_PLAN_PLAIN")
}
"#,
    );
    assert_success(
        "initial legacy saved-plan fixture apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    let resource = state["resources"]
        .as_array_mut()
        .expect("state resources")
        .iter_mut()
        .find(|resource| resource["identity"] == "legacy_plan")
        .expect("legacy_plan state row");
    resource["attributes"]["comment"] = Value::String(legacy_marker(
        &Value::String("LEGACY_SAVED_PLAN_PLAIN".to_string()),
        "mock.test.resource",
        "legacy_plan",
        "comment",
    ));
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).expect("encode legacy state"),
    )
    .expect("write legacy state");
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy_plan = mock.test.resource {
  name        = "legacy-saved-plan"
  comment     = secret("LEGACY_SAVED_PLAN_PLAIN")
  web_acl_arn = "force-saved-plan-writeback"
}
"#,
    );

    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "legacy-plan.json", "."]);
    assert_plan_counts(&plan, 0, 1, 0);
    assert_success(
        "apply saved plan from legacy state",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "legacy-plan.json",
        ]),
    );

    let digest = comment_digest(&scenario.state_json(), "legacy_plan");
    assert!(
        digest.starts_with(V2_SECRET_PREFIX),
        "saved-plan writeback did not upgrade the legacy marker to a v2 digest: {digest}",
    );
    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
}

#[test]
fn changed_plaintext_against_legacy_digest_is_still_an_update() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy_changed = mock.test.resource {
  name    = "legacy-changed"
  comment = secret("LEGACY_BEFORE")
}
"#,
    );
    assert_success(
        "initial changed legacy apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    let resource = state["resources"]
        .as_array_mut()
        .expect("state resources")
        .iter_mut()
        .find(|resource| resource["identity"] == "legacy_changed")
        .expect("legacy_changed state row");
    resource["attributes"]["comment"] = Value::String(legacy_marker(
        &Value::String("LEGACY_BEFORE".to_string()),
        "mock.test.resource",
        "legacy_changed",
        "comment",
    ));
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).expect("encode changed legacy state"),
    )
    .expect("write changed legacy state");
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy_changed = mock.test.resource {
  name    = "legacy-changed"
  comment = secret("LEGACY_AFTER")
}
"#,
    );

    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 1, 0);
}

#[test]
fn matching_legacy_digest_in_create_only_attribute_does_not_replace() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let legacy_role = mock.iam.Role {
  role_name = "legacy-role"
  path      = secret("/legacy-path/")
}
"#,
    );
    assert_success(
        "initial create-only legacy apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let mut state = scenario.state_json();
    downgrade_state_to_v13(&mut state);
    let resource = state["resources"]
        .as_array_mut()
        .expect("state resources")
        .iter_mut()
        .find(|resource| resource["identity"] == "legacy_role")
        .expect("legacy_role state row");
    resource["attributes"]["path"] = Value::String(legacy_marker(
        &Value::String("/legacy-path/".to_string()),
        "mock.iam.Role",
        "legacy_role",
        "path",
    ));
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).expect("encode create-only legacy state"),
    )
    .expect("write create-only legacy state");

    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 0, 0);
}

#[test]
fn changed_secret_still_plans_an_update() {
    let scenario = Scenario::new(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let changed = mock.test.resource {
  name    = "changed-secret"
  comment = secret("BEFORE_PLAIN")
}
"#,
    );
    assert_success(
        "initial changed-secret apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    scenario.write_main(
        r#"backend local { path = "carina.state.json" }
provider mock {}

let changed = mock.test.resource {
  name    = "changed-secret"
  comment = secret("AFTER_PLAIN")
}
"#,
    );

    assert_plan_counts(&scenario.carina(&["plan", "--refresh=false", "."]), 0, 1, 0);
}
