//! carina#4479. projection and compatibility command coverage.
//! The native Sink schema and request handling come from report18's fixture.
use super::{LoadedProviderFactory, TestCommandRuntime, in_child_output};
use carina_core::provider::*;
use carina_core::resource::{ConcreteValue, ResourceId, Value};
use carina_core::schema::*;
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const AUTHORED: &str = "update-private-token";
const DIFFERENT: &str = "nekot-etavirp-etadpu";

#[derive(Clone)]
struct Fixture(Arc<Mutex<FixtureState>>);
struct FixtureState {
    payload: Json,
    string_list: bool,
    revision: i64,
    trace: Vec<String>,
    fail_update: bool,
    error_echo: &'static str,
}
impl Fixture {
    fn new(payload: Json, string_list: bool) -> Self {
        Self(Arc::new(Mutex::new(FixtureState {
            payload,
            string_list,
            revision: 1,
            trace: Vec::new(),
            fail_update: false,
            error_echo: DIFFERENT,
        })))
    }
    fn factories(&self) -> Vec<LoadedProviderFactory> {
        LoadedProviderFactory::for_test(vec![Box::new(self.clone())])
    }
    fn observed(&self, operation: &str) -> HashMap<String, Value> {
        let mut state = self.0.lock().unwrap();
        state.trace.push(operation.into());
        let payload = if state.string_list {
            // The real WASM-shaped read conversion must retain StringList.
            let stored = carina_core::protocol_convert::protocol_to_core_persisted_value(
                &carina_provider_protocol::types::PersistedValue::StringList(
                    state
                        .payload
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| item.as_str().unwrap().to_owned())
                        .collect(),
                ),
            )
            .unwrap();
            let value = stored.to_value_at("test.Sink.item.config.payload").unwrap();
            assert!(matches!(
                value,
                Value::Concrete(ConcreteValue::StringList(_))
            ));
            value
        } else {
            carina_core::value::json_to_dsl_value(&state.payload)
                .unwrap()
                .unwrap()
        };
        HashMap::from([(
            "config".into(),
            Value::Concrete(ConcreteValue::Map(
                [
                    (
                        "revision".into(),
                        Value::Concrete(ConcreteValue::Int(state.revision)),
                    ),
                    ("payload".into(), payload),
                ]
                .into_iter()
                .collect(),
            )),
        )])
    }
    fn trace(&self, expected: &[&str]) {
        let mut state = self.0.lock().unwrap();
        assert_eq!(state.trace, expected, "must reach the real provider path");
        state.trace.clear();
    }
}
impl ProviderFactory for Fixture {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "Observed graft native fixture"
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let provider = self.clone();
        Box::pin(
            async move { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) },
        )
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("Sink")
                .attribute(
                    AttributeSchema::required(
                        "config",
                        AttributeType::struct_(
                            "Config",
                            vec![
                                StructField::required("revision", AttributeType::int()).build(),
                                StructField::required(
                                    "payload",
                                    AttributeType::union(vec![
                                        AttributeType::map(AttributeType::string()),
                                        AttributeType::list(AttributeType::refined_string(
                                            None,
                                            Some(PatternSpec::new("allowed", PatternMatch::Full)),
                                            None,
                                            None,
                                        )),
                                    ]),
                                )
                                .build(),
                            ],
                        ),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::provider_populated(
                        "members",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        ]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}
impl Provider for Fixture {
    fn name(&self) -> &str {
        "test"
    }
    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        unreachable!()
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        unreachable!()
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn create(
        &self,
        _: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        self.0.lock().unwrap().trace.push("create".into());
        let attributes = request
            .resource()
            .wire_attributes()
            .transport_iter()
            .map(|(key, value)| {
                let json = serde_json::to_value(value.value()).unwrap();
                (
                    key.to_owned(),
                    carina_core::value::json_to_dsl_value(&json)
                        .unwrap()
                        .unwrap(),
                )
            })
            .collect();
        Box::pin(async move {
            Ok(attempt
                .issued(
                    AppliedIdentifier::new("test-sink-item").unwrap(),
                    Sent::AllRequestAttributes,
                )
                .succeeded(attributes))
        })
    }
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let state =
            AppliedState::from_identifier(id.clone(), identifier.clone(), self.observed("read"));
        Box::pin(async move { Ok(ReadOutcome::found(state)) })
    }
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _: UpdateRequest,
        _: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        if self.0.lock().unwrap().fail_update {
            self.0.lock().unwrap().trace.push("update".into());
            let echo = self.0.lock().unwrap().error_echo;
            return Box::pin(async move {
                Err(ProviderError::api_error(format!(
                    "native update rejected observed original {echo}"
                )))
            });
        }
        self.0.lock().unwrap().revision = 2;
        let state =
            AppliedState::from_identifier(id.clone(), identifier.clone(), self.observed("update"));
        Box::pin(async move { Ok(UpdateOutcome::success(state)) })
    }
    fn delete(&self, _: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        self.0.lock().unwrap().trace.push("delete".into());
        Box::pin(async { Ok(()) })
    }
}

struct Scene {
    dir: tempfile::TempDir,
    runtime: TestCommandRuntime,
    fixture: Fixture,
    authored: String,
    ignored: bool,
}
impl Scene {
    async fn seed(payload: Json, authored: &str, ignored: bool, string_list: bool) -> Self {
        let scene = Self {
            dir: tempfile::tempdir().unwrap(),
            runtime: TestCommandRuntime::new(),
            fixture: Fixture::new(payload, string_list),
            authored: authored.into(),
            ignored,
        };
        let path = scene.dir.path().join("carina.state.json");
        std::fs::write(
            scene.dir.path().join("providers.crn"),
            "provider test { source = \"file://./native-fixture.wasm\" }\n",
        )
        .unwrap();
        std::fs::write(
            scene.dir.path().join("backend.crn"),
            format!("backend local {{ path = {} }}\n", json!(path)),
        )
        .unwrap();
        std::fs::write(
            scene.dir.path().join("carina-backend.lock"),
            json!({"backend_type":"local", "attributes":{"path":path}}).to_string(),
        )
        .unwrap();
        scene.source(1);
        scene.apply().await.unwrap();
        scene.fixture.trace(&["create"]);
        scene.source(2);
        scene
    }
    fn source(&self, revision: i64) {
        let directives = if self.ignored {
            "directives { ignore_changes = [config.payload] }"
        } else {
            ""
        };
        std::fs::write(self.dir.path().join("main.crn"), format!("let item = test.Sink {{\n config = {{ revision = {revision} payload = {} }}\n {directives}\n}}\n", self.authored)).unwrap();
    }
    #[allow(clippy::result_large_err)] // Keep the command's typed error for boundary diagnostics.
    async fn apply(&self) -> Result<(), carina_cli::error::AppError> {
        carina_cli::commands::apply::run_apply_with_factories_for_test(
            &self.runtime,
            self.dir.path(),
            self.fixture.factories(),
        )
        .await
    }
    async fn plan(&self, json_output: bool, saved: bool) {
        println!("OBSERVED_GRAFT_TARGET_PLAN");
        let out = saved.then(|| self.dir.path().join("plan.json"));
        assert!(
            carina_cli::test_support::run_observed_plan_with_factories(
                &self.runtime,
                self.dir.path(),
                out.as_deref(),
                true,
                json_output,
                self.fixture.factories()
            )
            .await
            .unwrap()
        );
        self.fixture.trace(&["read"]);
        if let Some(out) = out {
            let document: Json = serde_json::from_slice(&std::fs::read(out).unwrap()).unwrap();
            println!("OBSERVED_GRAFT_DOCUMENT={document}");
        }
    }
    #[allow(clippy::result_large_err)] // Keep the command's typed error for boundary diagnostics.
    async fn saved_apply(&self) -> Result<(), carina_cli::error::AppError> {
        println!("OBSERVED_GRAFT_TARGET_SAVED_APPLY");
        carina_cli::commands::apply::run_saved_apply_with_factories_for_test(
            &self.runtime,
            &self.dir.path().join("plan.json"),
            self.fixture.factories(),
        )
        .await
    }
    fn persisted_payload(&self) -> Json {
        let document: Json = serde_json::from_slice(
            &std::fs::read(self.dir.path().join("carina.state.json")).unwrap(),
        )
        .unwrap();
        document["resources"][0]["current"]["attributes"]["config"]["payload"].clone()
    }
}
const SECRET_MAP: &str = "{ original = secret('update-private-token') }";

// Matching secret leaves in the sent patch must enroll their observed spelling.
#[test]
fn apply_ignored_matching_map_update_error_masks_different_observed_spelling() {
    if let Some((stdout, stderr)) = captured(
        "apply_ignored_matching_map_update_error_masks_different_observed_spelling",
        async || {
            let scene = Scene::seed(json!({"original": DIFFERENT}), SECRET_MAP, true, false).await;
            scene.fixture.0.lock().unwrap().fail_update = true;
            print_error(scene.apply().await.expect_err("native update must fail"));
            scene.fixture.trace(&["read", "update", "read"]);
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, DIFFERENT);
        assert!(
            stderr.contains("native update rejected observed original (secret)"),
            "{stderr}"
        );
    }
}

fn captured(name: &str, exercise: impl AsyncFnOnce()) -> Option<(String, String)> {
    in_child_output(&format!("observed_graft::{name}"), || {
        tokio::runtime::Runtime::new().unwrap().block_on(exercise());
    })
    .map(|output| {
        (
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    })
}
fn document(stdout: &str) -> Json {
    serde_json::from_str(
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("OBSERVED_GRAFT_DOCUMENT="))
            .expect("saved document marker"),
    )
    .unwrap()
}
fn snapshot_payloads(document: &Json) -> [&Json; 2] {
    let old = &document["plan"]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|effect| effect.get("Update"))
        .unwrap()["from"]["attributes"]["config"]["Map"]["payload"];
    let current = &document["current_states"][0]["current"]["state"]["attributes"]["config"]["Map"]
        ["payload"];
    [old, current]
}
fn assert_hash(value: &Json, expected: &Json) -> String {
    let digest = value["RedactedSecret"]["hash"]
        .as_str()
        .unwrap_or_else(|| panic!("observed plaintext was not hashed: {value}"));
    let payload = carina_core::value::json_to_dsl_value(expected)
        .unwrap()
        .unwrap();
    let saved: carina_core::resource::SnapshotValue =
        serde_json::from_value(value.clone()).unwrap();
    assert!(
        saved.verify_secret(&payload).unwrap(),
        "hash must cover the observed subtree"
    );
    digest.into()
}
fn assert_no_plaintext(stdout: &str, stderr: &str, item: &str) {
    assert!(
        !format!("{stdout}{stderr}").contains(item),
        "observed secret leaked:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
fn print_error(error: carina_cli::error::AppError) {
    eprintln!(
        "Error: {}",
        carina_cli::error::format_error_with_source_chain(&error)
    );
}

macro_rules! invalid_apply {
    ($name:ident, $item:expr) => {
        // RED: the mismatched map/list diagnostic prints the invalid item.
        #[test]
        fn $name() {
            if let Some((stdout, stderr)) = captured(stringify!($name), async || {
                let scene = Scene::seed(json!([$item]), SECRET_MAP, true, false).await;
                let error = scene.apply().await.unwrap_err();
                scene.fixture.trace(&["read"]);
                print_error(error);
            }) {
                assert_no_plaintext(&stdout, &stderr, $item);
                assert!(
                    stderr.contains("(secret)") && stderr.contains("config"),
                    "{stderr}"
                );
                assert!(
                    stderr.contains("from observed update patch"),
                    "missing admission provenance: {stderr}"
                );
            }
        }
    };
}
invalid_apply!(
    apply_ignored_secret_invalid_list_same_masks_observed_item,
    AUTHORED
);
invalid_apply!(
    apply_ignored_secret_invalid_list_different_masks_observed_item,
    DIFFERENT
);

macro_rules! json_case {
    ($name:ident, $payload:expr, $authored:expr, $ignored:expr, $string_list:expr, $check:expr) => {
        #[test]
        fn $name() {
            if let Some((stdout, stderr)) = captured(stringify!($name), async || {
                let scene = Scene::seed($payload, $authored, $ignored, $string_list).await;
                scene.plan(true, true).await;
            }) {
                let saved = document(&stdout);
                assert_eq!(saved["plan"]["effects"][0]["Update"]["from"]["attributes"]["config"]["Map"]["revision"]["Int"], 1);
                let snapshots = snapshot_payloads(&saved);
                assert_eq!(snapshots[0], snapshots[1], "duplicate locations must share the protected snapshot");
                for payload in snapshots { ($check)(payload); }
                assert_no_plaintext(&stdout, &stderr, AUTHORED);
                assert_no_plaintext(&stdout, &stderr, DIFFERENT);
            }
        }
    }
}
// RED: both JSON historical locations contain the observed list in plaintext.
json_case!(
    plan_json_ignored_secret_invalid_list_same_masks_both_snapshots,
    json!([AUTHORED]),
    SECRET_MAP,
    true,
    false,
    |p| {
        assert_hash(p, &json!([AUTHORED]));
    }
);
// RED.
json_case!(
    plan_json_ignored_secret_invalid_list_different_masks_both_snapshots,
    json!([DIFFERENT]),
    SECRET_MAP,
    true,
    false,
    |p| {
        assert_hash(p, &json!([DIFFERENT]));
    }
);
// RED: a valid observed list is protected even though it passes the schema.
json_case!(
    plan_json_ignored_secret_valid_list_hashes_whole_payload,
    json!(["allowed"]),
    SECRET_MAP,
    true,
    false,
    |p| {
        assert_hash(p, &json!(["allowed"]));
    }
);
// Sent Whole units must remain protected: sent Whole units enroll their strings.
#[test]
fn apply_ignored_secret_valid_list_update_error_masks_whole_payload_string() {
    if let Some((stdout, stderr)) = captured(
        "apply_ignored_secret_valid_list_update_error_masks_whole_payload_string",
        async || {
            let scene = Scene::seed(json!(["allowed"]), SECRET_MAP, true, false).await;
            {
                let mut fixture = scene.fixture.0.lock().unwrap();
                fixture.fail_update = true;
                fixture.error_echo = "allowed";
            }
            print_error(scene.apply().await.expect_err("native update must fail"));
            scene.fixture.trace(&["read", "update", "read"]);
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, "allowed");
        assert!(
            stderr.contains("native update rejected observed original (secret)"),
            "{stderr}"
        );
    }
}
// PIN: matching maps keep leaf-level hashes of the observed value.
json_case!(
    plan_json_ignored_secret_matching_map_same_keeps_leaf_hash,
    json!({"original":AUTHORED}),
    SECRET_MAP,
    true,
    false,
    |p: &Json| {
        assert_hash(&p["Map"]["original"], &json!(AUTHORED));
    }
);
// PIN.
json_case!(
    plan_json_ignored_secret_matching_map_different_keeps_leaf_hash,
    json!({"original":DIFFERENT}),
    SECRET_MAP,
    true,
    false,
    |p: &Json| {
        assert_hash(&p["Map"]["original"], &json!(DIFFERENT));
    }
);
// PIN (A2): extra observed keys do not promote a matching secret-bearing map.
json_case!(
    plan_json_extra_observed_map_key_preserves_public_siblings,
    json!({"k":DIFFERENT,"env":"prod","extra":"public-extra"}),
    "{ k = secret('update-private-token'), env = 'prod' }",
    true,
    false,
    |p: &Json| {
        assert_hash(&p["Map"]["k"], &json!(DIFFERENT));
        assert_eq!(p["Map"]["env"]["String"], "prod");
        assert_eq!(p["Map"]["extra"]["String"], "public-extra");
    }
);
// RED (A6): the nonignored JSON history must remain protected.
json_case!(
    plan_json_nonignored_divergent_secret_masks_update_and_current,
    json!([DIFFERENT]),
    SECRET_MAP,
    false,
    false,
    |p| {
        assert_hash(p, &json!([DIFFERENT]));
    }
);
// RED (A6): WASM StringList, same positions/length, authored leaf secret.
json_case!(
    plan_json_wasm_string_list_matching_positions_masks_secret,
    json!(["allowed"]),
    "[secret('allowed')]",
    true,
    true,
    |p: &Json| {
        assert!(p.get("List").is_some(), "StringList secret leaked: {p}");
        assert_hash(&p["List"][0], &json!("allowed"));
    }
);

// PIN: ordinary observations and their validation messages retain their values.
#[test]
fn observed_list_without_authored_secret_stays_ordinary() {
    if let Some((stdout, stderr)) = captured(
        "observed_list_without_authored_secret_stays_ordinary",
        async || {
            let scene =
                Scene::seed(json!([DIFFERENT]), "{ original = 'ordinary' }", true, false).await;
            scene.plan(true, true).await;
            let error = scene.apply().await.unwrap_err();
            scene.fixture.trace(&["read"]);
            print_error(error);
        },
    ) {
        for p in snapshot_payloads(&document(&stdout)) {
            assert_eq!(p, &json!({"List":[{"String":DIFFERENT}]}));
        }
        assert!(
            stderr.contains(DIFFERENT),
            "ordinary diagnostic lost its value: {stderr}"
        );
    }
}
// RED (A6): nonignored text update rows must mask only the payload.
#[test]
fn text_plan_nonignored_divergent_secret_masks_update_rows() {
    if let Some((stdout, stderr)) = captured(
        "text_plan_nonignored_divergent_secret_masks_update_rows",
        async || {
            Scene::seed(json!([DIFFERENT]), SECRET_MAP, false, false)
                .await
                .plan(false, false)
                .await;
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, DIFFERENT);
        let target = stdout.split("OBSERVED_GRAFT_TARGET_PLAN").last().unwrap();
        assert!(
            target.contains("payload")
                && target.contains("(secret)")
                && target.contains("revision")
                && target.contains('2'),
            "{target}"
        );
    }
}
// RED (A6): destroy and orphan delete rows use the persisted Map hash.
macro_rules! delete_case {
    ($name:ident, $orphan:expr) => {
        #[test]
        fn $name() {
            if let Some((stdout, stderr)) = captured(stringify!($name), async || {
                let scene = Scene::seed(json!([DIFFERENT]), SECRET_MAP, false, false).await;
                println!("OBSERVED_GRAFT_TARGET_DELETE");
                if $orphan {
                    std::fs::write(scene.dir.path().join("main.crn"), "").unwrap();
                    scene.plan(false, false).await;
                } else {
                    carina_cli::commands::destroy::run_destroy_with_factories_for_test(
                        &scene.runtime,
                        scene.dir.path(),
                        scene.fixture.factories(),
                    )
                    .await
                    .unwrap();
                    scene.fixture.trace(&["read", "delete"]);
                }
            }) {
                assert_no_plaintext(&stdout, &stderr, DIFFERENT);
                let target = stdout.split("OBSERVED_GRAFT_TARGET_DELETE").last().unwrap();
                assert!(
                    target.contains("payload")
                        && target.contains("(secret)")
                        && target.contains("revision"),
                    "{target}"
                );
            }
        }
    };
}
delete_case!(destroy_divergent_secret_masks_delete_rows, false);
delete_case!(orphan_divergent_secret_masks_delete_rows, true);

// RED (A1): keep only the saved-file secrecy assertion, no replay redesign.
#[test]
fn saved_plan_ignored_secret_divergence_contains_no_plaintext() {
    if let Some((stdout, stderr)) = captured(
        "saved_plan_ignored_secret_divergence_contains_no_plaintext",
        async || {
            Scene::seed(json!([DIFFERENT]), SECRET_MAP, true, false)
                .await
                .plan(false, true)
                .await;
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, DIFFERENT);
        for p in snapshot_payloads(&document(&stdout)) {
            assert_hash(p, &json!([DIFFERENT]));
        }
    }
}
// PIN (A3): all three valid report18 controls preserve the seed's authored hash.
#[test]
fn valid_observed_controls_preserve_authored_state_hash() {
    captured(
        "valid_observed_controls_preserve_authored_state_hash",
        async || {
            for payload in [
                json!(["allowed"]),
                json!({"original":AUTHORED}),
                json!({"original":DIFFERENT}),
            ] {
                let scene = Scene::seed(payload, SECRET_MAP, true, false).await;
                let before = scene.persisted_payload();
                assert!(!before.is_null() && before.to_string().contains("$secret"));
                scene.apply().await.unwrap();
                scene.fixture.trace(&["read", "update"]);
                assert_eq!(scene.persisted_payload(), before);
            }
        },
    );
}
// RED (A6): saved-apply drift is printed through the full command.
#[test]
fn saved_apply_nonignored_divergent_secret_masks_drift_text() {
    if let Some((stdout, stderr)) = captured(
        "saved_apply_nonignored_divergent_secret_masks_drift_text",
        async || {
            let scene = Scene::seed(json!({"original":AUTHORED}), SECRET_MAP, false, false).await;
            scene.plan(false, true).await;
            scene.fixture.0.lock().unwrap().payload = json!([DIFFERENT]);
            let error = scene.saved_apply().await.unwrap_err();
            scene.fixture.trace(&["read"]);
            print_error(error);
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, DIFFERENT);
        let target = stdout
            .split("OBSERVED_GRAFT_TARGET_SAVED_APPLY")
            .last()
            .unwrap();
        assert!(
            target.contains("Infrastructure drift detected!") && target.contains("(secret)"),
            "{target}\n{stderr}"
        );
    }
}
// PIN / A1 one-time measurement: matching ignored secret drift still refuses.
#[test]
fn pin_saved_apply_ignored_matching_secret_verification_is_ignore_unaware() {
    if let Some((stdout, stderr)) = captured(
        "pin_saved_apply_ignored_matching_secret_verification_is_ignore_unaware",
        async || {
            let scene = Scene::seed(json!({"original":AUTHORED}), SECRET_MAP, true, false).await;
            scene.plan(false, true).await;
            scene.fixture.0.lock().unwrap().payload = json!({"original":DIFFERENT});
            let error = scene.saved_apply().await.unwrap_err();
            scene.fixture.trace(&["read"]);
            print_error(error);
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, DIFFERENT);
        let target = stdout
            .split("OBSERVED_GRAFT_TARGET_SAVED_APPLY")
            .last()
            .unwrap();
        assert!(
            target.contains("Infrastructure drift detected!") && target.contains("(secret)"),
            "{target}\n{stderr}"
        );
    }
}

// PIN: ignored payload rows stay omitted; only the unrelated revision is shown.
#[test]
fn text_plan_ignored_secret_keeps_payload_row_omitted() {
    if let Some((stdout, stderr)) = captured(
        "text_plan_ignored_secret_keeps_payload_row_omitted",
        async || {
            Scene::seed(json!([DIFFERENT]), SECRET_MAP, true, false)
                .await
                .plan(false, false)
                .await;
        },
    ) {
        assert_no_plaintext(&stdout, &stderr, DIFFERENT);
        let target = stdout.split("OBSERVED_GRAFT_TARGET_PLAN").last().unwrap();
        assert!(
            target.contains("ignored attribute(s) differ") && target.contains("revision"),
            "{target}"
        );
        assert!(!target.contains("~ payload"), "{target}");
    }
}

// Deferred replacement history must use the production JSON/saved-plan owner.
#[test]
fn plan_json_saved_deferred_replace_divergent_history_masks_observed_list() {
    if let Some((stdout, stderr)) = captured(
        "plan_json_saved_deferred_replace_divergent_history_masks_observed_list",
        async || {
            let scene = Scene {
                dir: tempfile::tempdir().unwrap(),
                runtime: TestCommandRuntime::new(),
                fixture: Fixture::new(json!(["x"]), false),
                authored: SECRET_MAP.into(),
                ignored: false,
            };
            let state_path = scene.dir.path().join("carina.state.json");
            std::fs::write(
                scene.dir.path().join("providers.crn"),
                "provider test { source = 'file://./native-fixture.wasm' }\n",
            )
            .unwrap();
            std::fs::write(
                scene.dir.path().join("backend.crn"),
                format!("backend local {{ path = {} }}\n", json!(state_path)),
            )
            .unwrap();
            std::fs::write(
                scene.dir.path().join("carina-backend.lock"),
                json!({"backend_type":"local", "attributes":{"path":state_path}}).to_string(),
            )
            .unwrap();
            let child =
                format!("test.Sink {{ config = {{ revision = 1 payload = {SECRET_MAP} }} }}");
            std::fs::write(
                scene.dir.path().join("main.crn"),
                format!("let items = for member in ['seed'] {{ {child} }}\n"),
            )
            .unwrap();
            scene.apply().await.unwrap();
            scene.fixture.trace(&["create"]);
            std::fs::write(scene.dir.path().join("main.crn"), format!(
                "let producer = test.Sink {{ config = {{ revision = 1 payload = {{ public = 'ordinary' }} }} }}\nlet items = for member in producer.members {{ {child} }}\n"
            )).unwrap();
            scene.plan(true, true).await;
        },
    ) {
        let saved = document(&stdout);
        let deferred = saved["plan"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|effect| effect.get("DeferredReplace"))
            .expect("the command must produce a deferred replacement");
        let old = &deferred["deletes"][0]["previous_attributes"]["config"]["Map"]["payload"];
        assert_hash(old, &json!(["x"]));
        let encoded = serde_json::to_string(&saved).unwrap();
        assert!(
            !encoded.contains("\"String\":\"x\""),
            "observed history leaked: {encoded}"
        );
        assert_no_plaintext(&stdout, &stderr, AUTHORED);
        assert_no_plaintext(&stdout, &stderr, "\"x\"");
    }
}
